//! The executor. It walks the optimizer's wave schedule: for each wave it launches all
//! the fetches **concurrently** (this is where inferred parallelism becomes real wall
//! clock savings), awaits them, then runs the wave's non-fetch nodes in order. Control
//! flow (`match`, `flow` calls, `retry`) is handled here because it can do I/O.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use hale_compiler::ir::{ArmBodyIr, Body, FetchIr, MatchIr, NodeKind};
use hale_syntax::ast::{Effect, Expr};

use crate::eval::{apply_pipeline, eval, match_pattern, Env};
use crate::value::{ErrValue, Outcome, RunError, Value};
use crate::{contracts, Shared};

const MAX_RETRIES: usize = 5;

impl Shared {
    /// Execute a body, returning its value (the last expression / a `return`).
    pub(crate) async fn run_body(
        self: &Arc<Self>,
        body: &Body,
        env: &mut Env,
        active: &HashSet<String>,
    ) -> Result<Option<Value>, RunError> {
        let mut last_value: Option<Value> = None;

        for wave in &body.waves {
            // ---- launch every fetch in this wave concurrently ----
            let snapshot = env.clone();
            let mut handles = Vec::new();
            for &id in wave {
                if let NodeKind::Fetch(f) = &body.nodes[id].kind {
                    let shared = self.clone();
                    let fetch = f.clone();
                    let snap = snapshot.clone();
                    let act = active.clone();
                    let binding = body.nodes[id].binding.clone();
                    handles.push((
                        binding,
                        tokio::spawn(async move { shared.do_fetch(&fetch, &snap, &act).await }),
                    ));
                }
            }
            for (binding, handle) in handles {
                let value = handle
                    .await
                    .map_err(|e| RunError::new(format!("internal task failure: {e}")))??;
                if let Some(b) = binding {
                    env.insert(b, value);
                }
            }

            // ---- then the non-fetch nodes, in program order ----
            for &id in wave {
                let node = &body.nodes[id];
                match &node.kind {
                    NodeKind::Fetch(_) => {}
                    NodeKind::Let(e) => {
                        let v = eval(e, env, None)?;
                        if let Some(b) = &node.binding {
                            env.insert(b.clone(), v);
                        }
                    }
                    NodeKind::Log(e) => {
                        println!("{}", eval(e, env, None)?.display());
                    }
                    NodeKind::Assert(e) => {
                        if !eval(e, env, None)?.truthy() {
                            return Err(RunError::new(format!(
                                "assertion failed: {}",
                                hale_syntax::pretty::expr(e)
                            )));
                        }
                    }
                    NodeKind::Return(opt) => {
                        let v = match opt {
                            Some(e) => eval(e, env, None)?,
                            None => Value::Null,
                        };
                        return Ok(Some(v));
                    }
                    NodeKind::Expr(e) => {
                        let v = self.run_expr_node(e, env, active).await?;
                        if let Some(b) = &node.binding {
                            env.insert(b.clone(), v.clone());
                        }
                        last_value = Some(v);
                    }
                    NodeKind::Match(m) => {
                        let v = Box::pin(self.run_match(m, body, env, active)).await?;
                        if let Some(b) = &node.binding {
                            env.insert(b.clone(), v.clone());
                        }
                        last_value = Some(v);
                    }
                    NodeKind::ForEach(fe) => {
                        // Evaluate the collection once, then run the (already-optimized) body
                        // for each element with the loop variable bound. Iterations are
                        // sequential; independent fetches *within* one iteration still run as
                        // a parallel wave (the body has its own schedule). This is the very
                        // shape the N+1 analysis flags — and it really issues N requests.
                        let items = match eval(&fe.iter, env, None)? {
                            Value::Array(a) => a,
                            Value::Null => Vec::new(),
                            other => vec![other],
                        };
                        for item in items {
                            env.insert(fe.var.clone(), item);
                            let r = Box::pin(self.run_body(&fe.body, env, active)).await?;
                            // Propagate an explicit `return` out of the loop and its encloser.
                            if fe.returns && r.is_some() {
                                return Ok(r);
                            }
                        }
                    }
                    NodeKind::Scatter(s) => {
                        // Pick this binding's element out of the batched array by join key.
                        let want = eval(&s.value, env, None)?;
                        let picked = match env.get(&s.batch) {
                            Some(Value::Array(items)) => items
                                .iter()
                                .find(|el| el.get_field(&s.key_field) == want)
                                .cloned()
                                .unwrap_or(Value::Null),
                            _ => Value::Null,
                        };
                        if let Some(b) = &node.binding {
                            env.insert(b.clone(), picked);
                        }
                    }
                }
            }
        }
        Ok(last_value)
    }

    /// Run a statement-level expression. If it is a call to a declared `flow`, run that
    /// flow (it can do I/O); otherwise evaluate it synchronously.
    async fn run_expr_node(
        self: &Arc<Self>,
        e: &Expr,
        env: &Env,
        active: &HashSet<String>,
    ) -> Result<Value, RunError> {
        if let Expr::Call { callee, args, .. } = e {
            if let Expr::Ident(name) = callee.as_ref() {
                if self.compiled.flow(&name.node).is_some() {
                    let mut argv = Vec::new();
                    for a in args {
                        argv.push(eval(a, env, None)?);
                    }
                    return Box::pin(self.run_flow(&name.node, argv, active)).await;
                }
            }
        }
        eval(e, env, None)
    }

    /// Invoke a flow by name with already-evaluated arguments.
    pub(crate) async fn run_flow(
        self: &Arc<Self>,
        name: &str,
        args: Vec<Value>,
        active: &HashSet<String>,
    ) -> Result<Value, RunError> {
        let flow = self
            .compiled
            .flow(name)
            .ok_or_else(|| RunError::new(format!("unknown flow `{name}`")))?;
        let mut env = Env::new();
        for (p, v) in flow.params.iter().zip(args) {
            env.insert(p.clone(), v);
        }
        let r = Box::pin(self.run_body(&flow.body, &mut env, active)).await?;
        Ok(r.unwrap_or(Value::Null))
    }

    async fn run_match(
        self: &Arc<Self>,
        m: &MatchIr,
        body: &Body,
        env: &mut Env,
        active: &HashSet<String>,
    ) -> Result<Value, RunError> {
        for _attempt in 0..MAX_RETRIES {
            let scrut = eval(&m.scrutinee, env, None)?;
            let mut chosen = None;
            for arm in &m.arms {
                if let Some(binds) = match_pattern(&arm.pattern, &scrut) {
                    chosen = Some((arm, binds));
                    break;
                }
            }
            let (arm, binds) = chosen.ok_or_else(|| RunError::new("no match arm applied"))?;
            for (k, v) in binds {
                env.insert(k, v);
            }
            match &arm.body {
                ArmBodyIr::Value(e) => return eval(e, env, None),
                ArmBodyIr::Body(b) => {
                    let r = Box::pin(self.run_body(b, env, active)).await?;
                    return Ok(r.unwrap_or(Value::Null));
                }
                ArmBodyIr::Retry { effects } => {
                    self.run_effects(effects, env).await?;
                    self.refetch_scrutinee(m, body, env, active).await?;
                    // loop and re-match the refreshed scrutinee
                }
            }
        }
        // Retries exhausted: return whatever the scrutinee is now (likely the Err).
        eval(&m.scrutinee, env, None)
    }

    async fn run_effects(&self, effects: &[Effect], env: &Env) -> Result<(), RunError> {
        for eff in effects {
            match eff {
                Effect::Wait(e) => {
                    let ms = match eval(e, env, None)? {
                        Value::Int(n) => n.max(0) as u64,
                        Value::Duration(d) => d,
                        _ => 0,
                    };
                    tokio::time::sleep(Duration::from_millis(ms.min(2000))).await;
                }
                Effect::Call(e) => {
                    let _ = eval(e, env, None)?;
                }
            }
        }
        Ok(())
    }

    /// On `retry`, re-run the fetch that produced the matched variable, refreshing it.
    async fn refetch_scrutinee(
        self: &Arc<Self>,
        m: &MatchIr,
        body: &Body,
        env: &mut Env,
        active: &HashSet<String>,
    ) -> Result<(), RunError> {
        if let Expr::Ident(name) = &m.scrutinee {
            let fetch = body.nodes.iter().find_map(|n| match &n.kind {
                NodeKind::Fetch(f) if n.binding.as_deref() == Some(name.node.as_str()) => {
                    Some(f.clone())
                }
                _ => None,
            });
            if let Some(f) = fetch {
                let v = self.do_fetch(&f, env, active).await?;
                env.insert(name.node.clone(), v);
            }
        }
        Ok(())
    }

    /// Execute a single fetch: build the URL, route to a mock or the HTTP engine, run
    /// the pipeline, verify any contract, and wrap the outcome as the binding's value.
    pub(crate) async fn do_fetch(
        &self,
        f: &FetchIr,
        env: &Env,
        active: &HashSet<String>,
    ) -> Result<Value, RunError> {
        use hale_syntax::ast::PathSeg;

        let mut path = String::new();
        for seg in &f.path.segments {
            path.push('/');
            match seg {
                PathSeg::Literal(l) => path.push_str(l),
                PathSeg::Param(e) => path.push_str(&eval(e, env, None)?.display()),
            }
        }
        if path.is_empty() {
            path.push('/');
        }
        let mut query = Vec::new();
        for (k, e) in &f.params {
            query.push((k.clone(), eval(e, env, None)?.display()));
        }
        // A fused batch fetch carries its ids: join them into the declared query parameter,
        // e.g. `?ids=1,2,3`. Static fusion stores a fixed `ids` list; loop fusion stores a
        // `mapped` source — a key expression evaluated over a collection at runtime.
        // (An unfused candidate has neither and behaves as a plain GET.)
        if let Some(b) = &f.batch {
            let joined: Vec<String> = if let Some(m) = &b.mapped {
                let items = match eval(&m.coll, env, None)? {
                    Value::Array(a) => a,
                    Value::Null => Vec::new(),
                    other => vec![other],
                };
                // An empty collection means the loop would issue zero requests; send none
                // and return an empty batch so the in-loop scatters all yield nothing.
                if items.is_empty() {
                    return Ok(Value::Array(Vec::new()));
                }
                let mut out = Vec::with_capacity(items.len());
                for it in &items {
                    let mut child = env.clone();
                    child.insert(m.var.clone(), it.clone());
                    out.push(eval(&m.key, &child, Some(it))?.display());
                }
                out
            } else {
                let mut out = Vec::with_capacity(b.ids.len());
                for e in &b.ids {
                    out.push(eval(e, env, None)?.display());
                }
                out
            };
            if !joined.is_empty() {
                query.push((b.query_param.clone(), joined.join(",")));
            }
        }

        let body_json = match &f.body {
            Some(e) => Some(crate::record::value_to_json(&eval(e, env, None)?)),
            None => None,
        };
        let idempotency_key = match &f.idempotency_key {
            Some(e) => Some(eval(e, env, None)?.display()),
            None => None,
        };

        let key = crate::record::request_key(&f.method, &f.endpoint, &path, &query);

        let outcome = if self.record.is_replay() {
            // Replay: serve the recorded outcome; a missing key is a hard error so the
            // run stays deterministic and never silently falls back to the network.
            self.record.lookup(&key).ok_or_else(|| {
                RunError::new(format!(
                    "no recorded response for `{key}` — capture it with `--record` first"
                ))
            })?
        } else if active.contains(&f.endpoint) {
            match self.mocks.get(&f.endpoint) {
                Some(mock) => mock.lookup(&f.method, &path),
                None => Outcome::Failure(ErrValue::new(
                    "NotFound",
                    Some(404),
                    format!("no mock installed for `{}`", f.endpoint),
                )),
            }
        } else {
            let cfg = self
                .endpoints
                .get(&f.endpoint)
                .ok_or_else(|| RunError::new(format!("unknown endpoint `{}`", f.endpoint)))?;
            let out = self
                .http
                .request(
                    cfg,
                    &f.method,
                    &path,
                    &query,
                    body_json.as_ref(),
                    idempotency_key.as_deref(),
                )
                .await;
            self.record.store(key, &out); // no-op unless in record mode
            out
        };

        // Contract verification (successes only).
        if let Some(ty) = &f.contract_ty {
            if let Err(msg) =
                contracts::validate_outcome(&outcome, ty, &self.compiled.analysis.table)
            {
                return if f.as_result {
                    Ok(Value::Err(contracts::contract_failure(msg)))
                } else {
                    Err(RunError::new(msg))
                };
            }
        }

        match outcome {
            Outcome::Success(v) => {
                let piped = apply_pipeline(v, &f.pipeline, env)?;
                if f.as_result {
                    Ok(Value::Ok(Box::new(piped)))
                } else {
                    Ok(piped)
                }
            }
            Outcome::Failure(err) => {
                if f.as_result {
                    Ok(Value::Err(err))
                } else {
                    let status = err
                        .status
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "-".into());
                    Err(RunError::new(format!(
                        "request to `{}{}` failed: {} ({status}) — annotate the binding as `Result<...>` to handle it",
                        f.endpoint, path, err.variant
                    )))
                }
            }
        }
    }
}
