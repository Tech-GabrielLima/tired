# TIRED — *The Internet Request & Execution Domain-language*

> **Idiomas:** [English](README.md) · **Português**

> **APIs são cansativas. Então eu construí uma linguagem.**
>
> TIRED é uma pequena **linguagem de domínio específico, compilada, para consumir APIs HTTP**, feita do
> zero em Rust. Não é uma biblioteca cliente — é uma linguagem, com lexer, parser recursivo, type
> checker, uma IR em estilo SSA, um otimizador e um runtime concorrente. A ideia central: as coisas que
> você normalmente escreve à mão em volta de cada chamada de API — tratamento de erro, paralelismo,
> retries, validação — viram *propriedades da linguagem* que o compilador verifica e o otimizador
> explora.

```tired
endpoint GitHub {
  base:    "https://api.github.com"
  auth:    Bearer($GITHUB_TOKEN)
  timeout: 5s
  retry:   3 backoff(exponential)
  cache:   ttl(5min)
}

type Repo { name: String  stargazers_count: Integer where (>= 0) }

flow Dashboard(username: String) -> User {
  fetch GitHub /users/{username} -> user: User       // roda primeiro…

  fetch GitHub /users/{username}/repos               // …estas duas não dependem
    | sort(by: .stargazers_count desc) | limit(3)     //    uma da outra, então o
    -> top: Repo[]                                     //    otimizador as executa
  fetch GitHub /users/{username}/followers | limit(3) //    em paralelo, sozinho.
    -> followers

  log "{user.login}: {top.length} top repos, {followers.length} seguidores"
  return user
}
```

Você escreveu três `fetch` sequenciais. O otimizador do TIRED percebeu que os dois últimos são
independentes e os agendou de forma concorrente — sem `Promise.all`, sem `CompletableFuture`, sem
`asyncio.gather`.

---

## As três ideias que fazem disto uma linguagem, não uma biblioteca

### 1 · Tratamento de erro dependente da rede — verificado em tempo de compilação

Um `fetch` anotado como `Result<T, E>` *não pode ser usado como se tivesse dado certo*. Ler um campo
dele, ou esquecer de tratar uma falha, é **erro de compilação** — não existe `NullPointerException`
para descobrir às 3h da manhã.

```text
$ tired check examples/broken.tired

error: no field `starz` on type `Repo`
  --> examples/broken.tired:15:25
   = help: did you mean `stars`?

error: cannot read field `name` — `maybe` is a `Result<Repo, ?>`
   = help: `match` on it first and read the field inside the `Ok(...)` arm
   = note: the request might have failed; TIRED will not let you ignore that

error: unhandled error: `maybe` has type `Result<Repo, ?>` and may be an `Err`
   = help: `match maybe { ... }` and handle both `Ok` and `Err`, or `return maybe` to propagate it
```

Um `match` sobre um `Result` precisa ser **exaustivo**. Uma união de erros fechada
(`Result<T, NotFound | Unauthorized>`) obriga a cobrir cada variante; um tipo de erro aberto obriga a um
catch-all `Err(e) => …`.

### 2 · Inferência de paralelismo

O compilador rebaixa cada corpo para uma IR onde as dependências de dados são explícitas, e então
agenda os nós em **ondas topológicas**. Requisições independentes caem na mesma onda e executam de forma
concorrente — você nunca pediu por isso.

```text
$ tired explain examples/parallel.tired

main:
  wave 1:  ‖ 3 requests in parallel
    • fetch GitHub /users/torvalds -> a
    • fetch GitHub /users/octocat -> b
    • fetch GitHub /users/gvanrossum -> c
  wave 2:
    • log
```

### 3 · Eliminação de requisições mortas

Um `fetch` cujo resultado nunca é observado é **removido antes de qualquer byte ir para a rede** — zero
bytes enviados — e reportado como aviso. (Descoberto organicamente ao montar o benchmark: um `log` que
não referenciava os valores buscados fez o otimizador eliminar *todas* as requisições, o que é
exatamente o comportamento correto.)

```text
warning: request `GitHub /users/torvalds/repos` is never used and was eliminated
   = note: dead-request elimination: 0 bytes were sent for it
```

### 4 · Deduplicação de requisições (CSE)

Dois `fetch`es que disparam a requisição **idêntica** (mesmo endpoint, path, params, pipeline *e* as
mesmas entradas) são colapsados — o segundo reusa o resultado do primeiro, então a mesma URL nunca é
chamada duas vezes. É eliminação de subexpressão comum, para a rede. O `tired explain` mostra o segundo
fetch reescrito como `let`:

```text
main:
  wave 1:
    • fetch GitHub /users/octocat -> a
  wave 2:
    • let -> b          # requisição idêntica — reusa `a`, 0 bytes extras
```

O pipeline também é rico: `filter` · `map`/`pluck` · `sort` · `limit`/`take` · `skip` · `reverse` ·
`unique` · `flatten` · `count` · `sum`.

---

## Por que uma linguagem — e como ela se compara

Bibliotecas de cliente são ótimas. A aposta do TIRED é que as partes *recorrentes e perigosas* de
consumir uma API — paralelismo, tratamento de erro, retries, validação, testes — não deveriam ser
redigitadas à mão em todo projeto. Elas deveriam ser **propriedades que o compilador verifica e o
otimizador explora**.

| | `requests`/`httpx` (Py) | `fetch`/`axios` (JS) | Feign/RestTemplate (Java) | **TIRED** |
|---|:---:|:---:|:---:|:---:|
| Chamadas independentes em paralelo **automaticamente** | ✗ (`gather` manual) | ✗ (`Promise.all` manual) | ✗ | **✓** |
| **Não compila** se você ignorar um erro possível | ✗ | ✗ | ✗ | **✓** |
| Requisições idênticas **dedupadas**; não usadas **removidas** | ✗ | ✗ | ✗ | **✓** |
| Retry / backoff / timeout / cache como **config declarativa** | manual | manual | anotações | **✓** |
| **Mocks** na linguagem + testes (offline, determinísticos) | libs à parte | libs à parte | parcial | **✓** |
| **Record/replay** para execução offline determinística | ✗ | ✗ | ✗ | **✓** |
| Validação de **contrato** das respostas em runtime | ✗ | ✗ | ✗ | **✓** |
| **Inferência** de schema + export **JSON Schema** | ✗ | ✗ | ✗ | **✓** |
| Um toolchain: type-check, `fmt`, LSP, plano de execução | n/a | n/a | n/a | **✓** |

**Por que usar:** o compilador se recusa a deixar um erro sem tratamento, o otimizador transforma seu
código sequencial no schedule seguro mais rápido (paralelo onde é independente, dedupado, chamadas mortas
removidas), e um único toolchain te dá formatação, language server, checagem de contratos, mocks e
record/replay — em vez de cinco bibliotecas coladas.

---

## O que foi construído vs. o que foi projetado

Este repositório é o **núcleo funcional** da linguagem — ele compila, faz type-check, otimiza e executa
programas reais contra APIs reais. A visão original do TIRED é um produto de vários anos e várias
equipes; as partes abaixo da linha estão deliberadamente **projetadas, mas não implementadas**, e eu
prefiro dizer isso a entregar stubs vazios.

| Construído e testado ✅ | Projetado, não implementado ⏳ |
|---|---|
| Lexer, parser, AST, diagnósticos estilo `rustc` (carets + "did you mean") | Bindings Java (JNI) |
| Type system + checker: `Result` exaustivo, tipagem de campos, resolução | Plugin IntelliJ (a extensão VS Code já está pronta) |
| IR + otimizador: **eliminação de requisições mortas**, **inferência de paralelismo**, **deduplicação** | Codegen WASM / nativo (LLVM), JIT adaptativo |
| **Análise estática de custo de requisições** (máx. de chamadas e paralelismo por rota/flow) | Modo cluster distribuído, registry TiredHub |
| Runtime concorrente: escalonador de ondas, HTTP/2, retry/backoff, timeout, auth bearer, cache TTL, métricas | Cache distribuído via Redis |
| **Verbos HTTP completos** (GET/POST/PUT/PATCH/DELETE) + body JSON; mutações nunca reordenadas/dedupadas/re-tentadas | Import de schema OpenAPI / GraphQL |
| **Modo `server`** — serve rotas HTTP cujos handlers consomem APIs (auto-paralelizados) | |
| **Mock engine** + blocos `test`; verificação de **contratos** em runtime | |
| **Language server** + **extensão VS Code**; **bindings Python** (PyO3, pip) | |
| **Time-travel** record & replay; **inferência de schema** + **export JSON Schema** | |
| CLI: `run`, `check`, `test`, `explain`, `fmt`, `inspect`, `schema`, `serve`, `replay`, `lsp` | |

---

## Medido aqui

`cargo test --workspace` → **51 testes + 1 doc-test, 0 falhas** em seis crates: lexer/parser, type
checker, otimizador (paralelismo, eliminação, deduplicação e custo de requisições), testes end-to-end de
runtime contra um servidor HTTP in-process — incluindo um **teste end-to-end do modo `server`** que sobe
um gateway TIRED e verifica que ele agrega dois upstreams em paralelo —, inferência + export de JSON
Schema, round-trip de record/replay e o language server. Os bindings Python (PyO3) compilam num módulo
`abi3` e são exercitados a partir do Python.

### Benchmark de inferência de paralelismo

```text
$ cargo test -p tired-runtime --test integration benchmark -- --nocapture

=== TIRED parallel-inference benchmark (6 fetches @ 100ms/hop) ===
  serial   (data-dependent chain): 620.1 ms
  parallel (independent, inferred): 104.7 ms
  speedup: 5.92x
```

> **Nota de honestidade.** Isto mede o *motor* contra um servidor in-process com latência por
> requisição fixa, injetada em software — caracteriza como o escalonador sobrepõe requisições, **não**
> uma rede de produção, e **não** é uma comparação com `httpx`/`reqwest`/`Feign` (não consigo rodá-los
> aqui). O número serial é uma baseline genuína: as mesmas seis requisições encadeadas por uma
> dependência de dados real, o que força uma onda por requisição. O que ele prova é estreito e
> verdadeiro — **requisições independentes escritas sequencialmente são executadas em paralelo, sem
> esforço do usuário.**

---

## Arquitetura

```
  source.tired
      │
      ▼   ┌─────────────────────────── tired-syntax (zero deps) ───────────────────────────┐
  Lexer → Parser → AST  ·  spans  ·  diagnósticos estilo rustc  ·  pretty-printer (tired fmt)
      │   └────────────────────────────────────────────────────────────────────────────────┘
      ▼   ┌────────────────────────── tired-compiler (zero deps) ─────────────────────────┐
  Type checker  →  lowering p/ IR  →  Otimizador
   · tratamento exaustivo de Result   · análise de variáveis livres / dependências
   · tipagem de campos + did-you-mean  · eliminação de requisições mortas
   · resolução de endpoint/variável    · inferência de paralelismo (ondas topológicas)
      │   └────────────────────────────────────────────────────────────────────────────────┘
      ▼   ┌────────────── tired-runtime (tokio + reqwest, as únicas deps) ─────────────────┐
  Executor de ondas ── dispara as requisições de cada onda concorrentemente
      ├── Motor HTTP: pool HTTP/2, retry+backoff, timeout, auth bearer, cache TTL, métricas
      ├── Motor de mock: roteamento offline e determinístico para `test`
      ├── Verificador de contratos: checagem de restrições `where` em runtime
      ├── Record/replay: captura respostas (`--record`) e as reproduz (`replay`)
      └────────────────────────────────────────────────────────────────────────────────────┘
            ▲ tired-lsp — language server (reusa o compilador): diagnostics · autocomplete · hover
            ▲ tired-cli — o binário `tired`: run · check · test · explain · fmt · inspect · replay · lsp
```

A separação é proposital: **todo o front-end do compilador é Rust std-only, sem dependências.** Apenas o
runtime — a parte que realmente precisa de uma stack HTTP assíncrona — usa `tokio` e `reqwest` (o LSP
reusa o compilador e só adiciona `serde_json`).

```
tired/
├── crates/
│   ├── tired-syntax/    lexer, parser, AST, diagnósticos, pretty-printer  (sem deps)
│   ├── tired-compiler/  tipos, checker, IR, otimizador                    (sem deps)
│   ├── tired-runtime/   eval, motores mock + HTTP, executor, contratos,
│   │                    inferência de schema, record/replay, servidor HTTP
│   ├── tired-lsp/       language server via stdio (diagnostics, autocomplete, hover)
│   ├── tired-py/        bindings Python (PyO3 / maturin)
│   └── tired-cli/       o driver de linha de comando `tired`
├── editors/vscode/      extensão VS Code (grammar + cliente LSP)
├── examples/            programas .tired executáveis (live + offline)
└── docs/                DESIGN.md e a gramática formal (grammar.ebnf)
```

---

## Como rodar

```bash
cargo build                              # compila o binário `tired`
alias tired="cargo run -q -p tired-cli --"

# Offline (sem rede) — o motor de mock + blocos de teste:
tired check   examples/broken.tired      # veja o compilador rejeitar código ruim
tired test    examples/mocked.tired      # pipeline + contratos, tudo mockado
tired test    examples/error_handling.tired
tired explain examples/parallel.tired    # mostra o plano paralelo inferido
tired fmt     examples/mocked.tired      # formatação canônica

# Live (usa a API pública do GitHub):
tired run examples/parallel.tired --show-plan --metrics
tired run examples/github_dashboard.tired --flow Dashboard octocat

# Inferência de schema — gera tipos TIRED de qualquer JSON:
tired inspect https://api.github.com/users/octocat User

# Time-travel: grave uma vez (live), reproduza pra sempre (offline, determinístico):
tired run    examples/parallel.tired --record session.json
tired replay session.json examples/parallel.tired

# modo server — TIRED como API gateway (handlers paralelizam os upstreams sozinhos):
tired explain examples/gateway.tired     # plano + custo de requisições, sem rede
tired serve   examples/gateway.tired     # serve em http://127.0.0.1:8088/api/...

# Language server (aponte o cliente LSP do seu editor pra cá):
tired lsp
```

Do **Python** (bindings PyO3): `pip install maturin && (cd crates/tired-py && maturin develop)`,
depois `import tired`.

Rodar a suíte de testes e o benchmark:

```bash
cargo test --workspace
cargo test -p tired-runtime --test integration benchmark -- --nocapture
```

---

## Sobre o nome

`TIRED` é um backronym — *The Internet Request & Execution Domain-language* — e uma piadinha: toda outra
forma de consumir uma API é um pouco cansativa. A linguagem não conserta a internet, mas faz o
compilador cuidar das partes chatas e propensas a erro por você.

---

*Código e comentários em inglês. Licença MIT. Um projeto de linguagem feito do zero — companheiro do
portfólio de sistemas (cudakit, nabla, nanollm) e dos backends (ledger, matching-engine, raftkv).*
