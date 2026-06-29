# hale — *HTTP API Language & Engine*

> **Idiomas:** [English](README.md) · **Português**
>
> **Docs:** [Referência da Linguagem](docs/LANGUAGE.md) · [Design & internals](docs/DESIGN.md) · [Gramática (EBNF)](docs/grammar.ebnf)

> ***hale*** *(adj., inglês)* — forte e saudável. Consumir uma API deveria deixar seu código assim.
>
> hale é uma pequena **linguagem de domínio específico, compilada, para consumir (e servir) APIs HTTP**,
> feita do zero em Rust. Não é uma biblioteca cliente — é uma linguagem, com lexer, parser recursivo, type
> checker, uma IR em estilo SSA, um otimizador e um runtime concorrente. A ideia central: as coisas que
> você normalmente escreve à mão em volta de cada chamada de API — tratamento de erro, paralelismo,
> retries, validação *e até o seu orçamento de latência/custo* — viram *propriedades da linguagem* que o
> compilador verifica e o otimizador explora.

```hale
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

Você escreveu três `fetch` sequenciais. O otimizador do hale percebeu que os dois últimos são
independentes e os agendou de forma concorrente — sem `Promise.all`, sem `CompletableFuture`, sem
`asyncio.gather`.

---

## As três ideias que fazem disto uma linguagem, não uma biblioteca

### 1 · Tratamento de erro dependente da rede — verificado em tempo de compilação

Um `fetch` anotado como `Result<T, E>` *não pode ser usado como se tivesse dado certo*. Ler um campo
dele, ou esquecer de tratar uma falha, é **erro de compilação** — não existe `NullPointerException`
para descobrir às 3h da manhã.

```text
$ hale check examples/broken.hale

error: no field `starz` on type `Repo`
  --> examples/broken.hale:15:25
   = help: did you mean `stars`?

error: cannot read field `name` — `maybe` is a `Result<Repo, ?>`
   = help: `match` on it first and read the field inside the `Ok(...)` arm
   = note: the request might have failed; hale will not let you ignore that

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
$ hale explain examples/parallel.hale

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
chamada duas vezes. É eliminação de subexpressão comum, para a rede. O `hale explain` mostra o segundo
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

### 4½ · Fusão de requisições / batching — vetorização para a rede 🧬

Se um endpoint declara uma regra `batch:`, o otimizador vai além da dedup: ele colapsa vários GETs que
diferem **só no último segmento do path** numa *única* chamada batched, e faz o **scatter** do array de
volta para cada binding pela chave de junção. N idas-e-voltas viram uma.

```hale
endpoint GH { base: "..."  batch: param("ids") key(.id) }

fetch GH /users/1 -> a   //   o otimizador funde estes três
fetch GH /users/2 -> b   //   numa ÚNICA  GET /users?ids=1,2,3
fetch GH /users/3 -> c   //   e faz o scatter do array por .id
```

```text
$ hale explain examples/batch.hale
  wave 1:
    • fetch GH /users?ids=… [batched ×3] -> __batch_0
  wave 2:
    • scatter __batch_0.id (from batch) -> a / -> b / -> c
```

A análise de custo então reporta `≤ 1 request` em vez de 3 — a rede é chamada uma vez. Nenhuma
biblioteca faz isso automaticamente; é loop fusion, para HTTP.

### 7 · Um SLA em tempo de compilação — `budget(...)` 🚀

Como o custo é conhecido estaticamente, você pode **afirmá-lo**. Anote um flow ou rota com um `budget` —
sobre requisições, fan-out, hops do caminho crítico, **ou `p99` (latência de relógio)** — e o compilador
se recusa a compilar se algum caminho puder excedê-lo:

```hale
endpoint Billing { base: "..."  latency: 120ms }     // latência declarada por hop

flow Overview(id: String) -> Customer budget(requests: 3, parallel: 2, hops: 2, p99: 400ms) {
  fetch Billing /customers/{id} -> customer: Customer
  fetch Billing /customers/{customer.id}/invoices      | count() -> invoices
  fetch Billing /customers/{customer.id}/subscriptions | count() -> subs
  return customer
}
```

```text
$ hale explain examples/sla.hale
flow Overview(id):  [≤ 3 requests, up to 2 in parallel, 2 hops deep, ~240ms critical path]
                    (budget: requests ≤ 3, parallel ≤ 2, hops ≤ 2, p99 ≤ 400ms)
  effects: reads {Billing}
```

`p99` é somado pelo caminho crítico a partir do `latency:` de cada endpoint; prometa uma latência que não
pode provar (um hop sem `latency:`) e o compilador avisa. Um orçamento que **vive no sistema de tipos**,
não um painel pós-incidente. O `hale explain` também imprime a **assinatura de efeito** provada
(`reads {Billing}` / `reads+writes {…}`).

### 7½ · Detecção de N+1 — o bug de performance nº 1 de cliente, pego em tempo de compilação 🪤

Buscar uma lista e, num loop, buscar uma coisa por elemento — o **N+1 query**. A hale acha isso com uma
**análise de fluxo de dados** de verdade: rastreia quais valores vieram da rede (o *1*) e quais derivam de
um elemento do `for` (o *N*), propagando essa proveniência por `let`s, braços de `match` e loops aninhados.

```hale
fetch GH /users -> users: User[]
for u in users {
  fetch GH /users/{u.id}/repos -> repos   // ← uma requisição por usuário
  log "{u.login}: {repos.length}"
}
```

```text
$ hale check examples/nplus1.hale
warning: N+1 query: `GH /users/{u.id}/repos` runs once per element of `users`
$ hale explain examples/nplus1.hale
flow Dashboard():  [unbounded requests — a fetch runs once per `for` element (N+1); …]
```

Ela enxerga através de uma indireção (`let id = u.id; fetch …/{id}`), reporta **loops aninhados** como
`Nˆ2` e sinaliza à parte uma leitura **invariante no loop** ("tire daqui"). Dois níveis: o lint é um
*aviso* (um N+1 sobre uma lista pequena pode ser aceitável), mas um fetch por elemento torna o
[custo](#6--ele-te-diz-o-custo-de-requisições--antes-de-você-subir) **ilimitado** — então qualquer
`budget(...)` naquele flow vira **erro de compilação**: você não pode prometer um limite que um loop
estoura.

E quando o endpoint permite, o compilador não só reclama — ele **conserta**. Veja §7¾.

### 7¾ · …e então conserta — fusão de loop automática 🔧

Se o endpoint por-elemento declara uma regra `batch:`, o otimizador **reescreve o loop**: tira o fetch
de dentro, junta todas as chaves numa única chamada batched e troca o fetch interno por um *scatter* puro.
`1 + N` viagens viram `1 + 1` — code motion de invariante de loop + batching, para a rede.

```hale
endpoint GH { base: "..."  batch: param("ids") key(.id) }
fetch GH /users -> users: User[]
for u in users {
  fetch GH /users/{u.id} -> detail   // ← fundido automaticamente, sem aviso: o compilador hoista
  log "{detail.login}"
}
```

```text
$ hale explain examples/loop_fusion.hale
flow Logins():  [≤ 2 requests, up to 1 in parallel, 2 hops deep]   ← era ilimitado (N+1)
  wave 2:  • fetch GH /users?ids=… [batched ×N, fused from loop] -> __loopbatch_0
  wave 3:  • for u in … (per-element loop)
             wave 1:  • scatter __loopbatch_0.id (from batch) -> detail
```

Um teste de runtime confirma que os três GETs por-elemento batem na rede **uma vez**.

### 8 · Controle de fluxo de informação — governança de dados no compilador 🔒

Rotule um campo como `PII` ou `Secret` e o compilador o **rastreia** num lattice `Public < PII < Secret`.
Cada endpoint tem uma `clearance:` (o dado mais sensível que pode receber); um `log` ou resposta HTTP só
tem clearance `Public`. O dado nunca pode fluir para um sink abaixo do seu rótulo:

```hale
type Customer { id: Integer  email: PII  card: Secret }
endpoint Analytics { base: "..."  clearance: Public }   // recusa PII/Secret
```

```text
$ hale check governance.hale
error: `PII`-labelled value `email` must not flow to the request to `Analytics` (cleared for `Public`)
```

Um segredo ainda pode fluir *para dentro* de um endpoint que precisa dele (clearance não declarada = topo),
e a checagem é transitiva. É GDPR / residência de dados / prevenção de vazamento, em tempo de compilação —
o mesmo motor de taint, agora um lattice de fluxo de informação de Denning.

### 9 · O compilador sabe quais escritas são seguras de repetir — `idempotent` ♻️

Uma mutação nunca é reenviada silenciosamente. Mas marque-a `idempotent(key: …)` e o hale *prova* que
repetir a escrita é seguro — reabilitando o retry e anexando um header `Idempotency-Key` (o modelo do
Stripe). Nenhuma biblioteca diz *"este `POST` é seguro de repetir e aquele não"*. O hale diz, porque está
no tipo.

```hale
fetch POST Billing /charges idempotent(key: order.id) body { ... } -> r
```

---

## Por que uma linguagem — e como ela se compara

Bibliotecas de cliente são ótimas. A aposta do hale é que as partes *recorrentes e perigosas* de
consumir uma API — paralelismo, tratamento de erro, retries, validação, testes — não deveriam ser
redigitadas à mão em todo projeto. Elas deveriam ser **propriedades que o compilador verifica e o
otimizador explora**.

| | `requests`/`httpx` (Py) | `fetch`/`axios` (JS) | Feign/RestTemplate (Java) | **hale** |
|---|:---:|:---:|:---:|:---:|
| Chamadas independentes em paralelo **automaticamente** | ✗ (`gather` manual) | ✗ (`Promise.all` manual) | ✗ | **✓** |
| **Não compila** se você ignorar um erro possível | ✗ | ✗ | ✗ | **✓** |
| Requisições idênticas **dedupadas**; não usadas **removidas** | ✗ | ✗ | ✗ | **✓** |
| GETs quase-idênticos **fundidos numa chamada batched** automaticamente | ✗ | ✗ | ✗ | **✓** |
| Retry / backoff / timeout / cache como **config declarativa** | manual | manual | anotações | **✓** |
| **Mocks** na linguagem + testes (offline, determinísticos) | libs à parte | libs à parte | parcial | **✓** |
| **Record/replay** para execução offline determinística | ✗ | ✗ | ✗ | **✓** |
| Validação de **contrato** das respostas em runtime | ✗ | ✗ | ✗ | **✓** |
| **Inferência** de schema + export **JSON Schema** | ✗ | ✗ | ✗ | **✓** |
| **Orçamento de requisições / latência (`p99`)** garantido *em tempo de compilação* | ✗ | ✗ | ✗ | **✓** |
| **Detecção de N+1 + fusão de loop automática** — fetch por elemento é sinalizado *e* batched | ✗ | ✗ | ✗ | **✓** |
| **Controle de fluxo de informação**: PII/Secret não chega a sink de menor clearance | ✗ | ✗ | ✗ | **✓** |
| **Segurança de retry no tipo**: prova quais escritas são `idempotent` | ✗ | ✗ | ✗ | **✓** |
| Um toolchain: type-check, `fmt`, LSP, plano de execução | n/a | n/a | n/a | **✓** |

**Por que usar:** o compilador se recusa a deixar um erro sem tratamento, o otimizador transforma seu
código sequencial no schedule seguro mais rápido (paralelo onde é independente, dedupado, chamadas mortas
removidas), e um único toolchain te dá formatação, language server, checagem de contratos, mocks e
record/replay — em vez de cinco bibliotecas coladas.

---

## O que foi construído vs. o que foi projetado

Este repositório é o **núcleo funcional** da linguagem — ele compila, faz type-check, otimiza e executa
programas reais contra APIs reais. A visão original do hale é um produto de vários anos e várias
equipes; as partes abaixo da linha estão deliberadamente **projetadas, mas não implementadas**, e eu
prefiro dizer isso a entregar stubs vazios.

| Construído e testado ✅ | Projetado, não implementado ⏳ |
|---|---|
| Lexer, parser, AST, diagnósticos estilo `rustc` (carets + "did you mean") | Import OpenAPI/GraphQL → endpoints+tipos; export OpenAPI/SDK |
| Type system + checker: `Result` exaustivo, tipagem de campos, resolução | Scheduling adaptativo guiado por profile (record/replay); paginação |
| IR + otimizador: **eliminação de requisições mortas**, **inferência de paralelismo**, **deduplicação** | Execução de ondas distribuída; registry haleHub; cache Redis |
| **Fusão/batching automático** (`/u/1`,`/u/2`→`/u?ids=1,2`, scatter por chave) | Fuzzing por propriedades a partir de contratos; tipos de frescor |
| **Detecção de N+1** (fluxo de dados) **+ fusão de loop automática** (`for` fetch → 1 chamada batched) | Bindings Java (JNI); plugin IntelliJ; codegen WASM/LLVM, JIT |
| **Análise estática de custo** (chamadas, paralelismo, hops do caminho crítico e latência `p99`) | |
| **SLA em tempo de compilação**: `budget(requests/parallel/hops/p99)` garantido pela análise | |
| **Controle de fluxo de informação**: lattice PII/Secret + `clearance` por endpoint | |
| **Idempotência / retry-safety no tipo** (`idempotent(key:)`); **assinaturas de efeito** | |
| Runtime concorrente: escalonador de ondas, HTTP/2, retry/backoff, timeout, auth bearer, cache TTL, métricas | |
| **Verbos HTTP completos** (GET/POST/PUT/PATCH/DELETE) + body JSON; segurança de mutação | |
| **Modo `server`** — serve rotas HTTP cujos handlers consomem APIs (auto-paralelizados) | |
| **Mock engine** + blocos `test`; verificação de **contratos** em runtime | |
| **Language server** + **extensão VS Code**; **bindings Python** (PyO3, pip) | |
| **Time-travel** record & replay; **inferência de schema** + **export JSON Schema** | |
| CLI: `run`, `check`, `test`, `explain`, `fmt`, `inspect`, `schema`, `serve`, `replay`, `lsp` | |

---

## Medido aqui

`cargo test --workspace` → **87 testes + 1 doc-test, 0 falhas** em seis crates: lexer/parser, type
checker (toda regra principal — `Result` exaustivo, **controle de fluxo de informação**, **budget /
`p99`**, **idempotência** — tem teste de aceitação e de rejeição), otimizador (paralelismo, eliminação,
deduplicação, **fusão de requisições**, **fusão de loop automática**, custo de requisições, hops e latência
do caminho crítico), **detecção de N+1** (1+N sobre lista buscada, detecção através de `let` intermediário,
`Nˆ2` em loops aninhados, leituras invariantes de loop, a rejeição budget→ilimitado e a supressão quando o
loop é auto-fundido), testes end-to-end de runtime contra um servidor HTTP in-process — incluindo um
**teste de fusão de loop** que verifica que um `for` por-elemento colapsa numa única chamada batched, um
**teste de laço `for`** que verifica o fan-out por elemento e um **teste end-to-end do modo `server`** que
sobe um gateway hale e verifica que ele agrega dois upstreams em paralelo —, inferência + export de JSON
Schema, round-trip de record/replay e o language server. Os bindings Python (PyO3) compilam num módulo
`abi3` e são exercitados a partir do Python.

### Benchmark de inferência de paralelismo

```text
$ cargo test -p hale-runtime --test integration benchmark -- --nocapture

=== hale parallel-inference benchmark (6 fetches @ 100ms/hop) ===
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
  source.hale
      │
      ▼   ┌─────────────────────────── hale-syntax (zero deps) ───────────────────────────┐
  Lexer → Parser → AST  ·  spans  ·  diagnósticos estilo rustc  ·  pretty-printer (hale fmt)
      │   └────────────────────────────────────────────────────────────────────────────────┘
      ▼   ┌────────────────────────── hale-compiler (zero deps) ─────────────────────────┐
  Type checker  →  lowering p/ IR  →  Otimizador
   · tratamento exaustivo de Result   · análise de variáveis livres / dependências
   · tipagem de campos + did-you-mean  · eliminação de requisições mortas
   · resolução de endpoint/variável    · inferência de paralelismo (ondas topológicas)
      │   └────────────────────────────────────────────────────────────────────────────────┘
      ▼   ┌────────────── hale-runtime (tokio + reqwest, as únicas deps) ─────────────────┐
  Executor de ondas ── dispara as requisições de cada onda concorrentemente
      ├── Motor HTTP: pool HTTP/2, retry+backoff, timeout, auth bearer, cache TTL, métricas
      ├── Motor de mock: roteamento offline e determinístico para `test`
      ├── Verificador de contratos: checagem de restrições `where` em runtime
      ├── Record/replay: captura respostas (`--record`) e as reproduz (`replay`)
      └────────────────────────────────────────────────────────────────────────────────────┘
            ▲ hale-lsp — language server (reusa o compilador): diagnostics · autocomplete · hover
            ▲ hale-cli — o binário `hale`: run · check · test · explain · fmt · inspect · replay · lsp
```

A separação é proposital: **todo o front-end do compilador é Rust std-only, sem dependências.** Apenas o
runtime — a parte que realmente precisa de uma stack HTTP assíncrona — usa `tokio` e `reqwest` (o LSP
reusa o compilador e só adiciona `serde_json`).

```
hale/
├── crates/
│   ├── hale-syntax/    lexer, parser, AST, diagnósticos, pretty-printer  (sem deps)
│   ├── hale-compiler/  tipos, checker, IR, otimizador                    (sem deps)
│   ├── hale-runtime/   eval, motores mock + HTTP, executor, contratos,
│   │                    inferência de schema, record/replay, servidor HTTP
│   ├── hale-lsp/       language server via stdio (diagnostics, autocomplete, hover)
│   ├── hale-py/        bindings Python (PyO3 / maturin)
│   └── hale-cli/       o driver de linha de comando `hale`
├── editors/vscode/      extensão VS Code (grammar + cliente LSP)
├── examples/            programas .hale executáveis (live + offline)
└── docs/                DESIGN.md e a gramática formal (grammar.ebnf)
```

---

## Como rodar

```bash
cargo build                              # compila o binário `hale`
alias hale="cargo run -q -p hale-cli --"

# Offline (sem rede) — o motor de mock + blocos de teste:
hale check   examples/broken.hale      # veja o compilador rejeitar código ruim
hale test    examples/mocked.hale      # pipeline + contratos, tudo mockado
hale test    examples/error_handling.hale
hale explain examples/parallel.hale    # mostra o plano paralelo inferido
hale fmt     examples/mocked.hale      # formatação canônica

# Live (usa a API pública do GitHub):
hale run examples/parallel.hale --show-plan --metrics
hale run examples/github_dashboard.hale --flow Dashboard octocat

# Inferência de schema — gera tipos hale de qualquer JSON:
hale inspect https://api.github.com/users/octocat User

# Time-travel: grave uma vez (live), reproduza pra sempre (offline, determinístico):
hale run    examples/parallel.hale --record session.json
hale replay session.json examples/parallel.hale

# modo server — hale como API gateway (handlers paralelizam os upstreams sozinhos):
hale explain examples/gateway.hale     # plano + custo de requisições, sem rede
hale serve   examples/gateway.hale     # serve em http://127.0.0.1:8088/api/...

# Language server (aponte o cliente LSP do seu editor pra cá):
hale lsp
```

Do **Python** (bindings PyO3): `pip install maturin && (cd crates/hale-py && maturin develop)`,
depois `import hale`.

Rodar a suíte de testes e o benchmark:

```bash
cargo test --workspace
cargo test -p hale-runtime --test integration benchmark -- --nocapture
```

---

## Sobre o nome

***hale*** *(adjetivo, inglês)* — livre de defeito ou doença; firme, robusto, *hale and hearty*. Também
é um backronym: **H**TTP **A**PI **L**anguage & **E**ngine. A aposta do projeto está na palavra: consumir
uma API costuma ser um pouco exaustivo — concorrência manual, checagens de erro esquecidas, latência
misteriosa, tokens vazados. O hale move esse trabalho para o compilador, de modo que o que você entrega
sai *robusto por construção*.

---

*Código e comentários em inglês. Licença MIT. Um projeto de linguagem feito do zero — companheiro do
portfólio de sistemas (cudakit, nabla, nanollm) e dos backends (ledger, matching-engine, raftkv).*
