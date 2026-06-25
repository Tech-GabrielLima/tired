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

---

## O que foi construído vs. o que foi projetado

Este repositório é o **núcleo funcional** da linguagem — ele compila, faz type-check, otimiza e executa
programas reais contra APIs reais. A visão original do TIRED é um produto de vários anos e várias
equipes; as partes abaixo da linha estão deliberadamente **projetadas, mas não implementadas**, e eu
prefiro dizer isso a entregar stubs vazios.

| Construído e testado ✅ | Projetado, não implementado ⏳ |
|---|---|
| Lexer, parser, AST, diagnósticos estilo `rustc` (carets + "did you mean") | Bindings Python / Java (PyO3 / JNI sobre um C ABI) |
| Type system + checker: `Result` exaustivo, tipagem de campos, resolução | LSP completo + plugins VS Code / IntelliJ |
| IR + otimizador: **eliminação de requisições mortas**, **inferência de paralelismo** | Codegen WASM / nativo (LLVM), JIT adaptativo |
| Runtime concorrente: escalonador de ondas, HTTP/2 via `reqwest`, retry/backoff, timeout, auth bearer, cache TTL, contadores estilo Prometheus | Modo cluster distribuído, registry TiredHub |
| **Mock engine** na própria linguagem + blocos `test` (offline, determinísticos) | Debug time-travel (record/replay), cache via Redis |
| Verificação de **contratos** em runtime (restrições `where`) | Inferência de schema / import OpenAPI & GraphQL, modo `server` |
| CLI: `run`, `check`, `test`, `explain`, `fmt` | |

---

## Medido aqui

`cargo test --workspace` → **36 testes + 1 doc-test, 0 falhas** nas quatro crates: lexer/parser, type
checker (cada regra-bandeira tem teste de aceitação e de rejeição), otimizador (paralelismo &
eliminação) e testes end-to-end de runtime contra um servidor HTTP in-process.

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
      └── Verificador de contratos: checagem de restrições `where` em runtime
      └────────────────────────────────────────────────────────────────────────────────────┘
            ▲ tired-cli — o binário `tired`: run · check · test · explain · fmt
```

A separação é proposital: **todo o front-end do compilador é Rust std-only, sem dependências.** Apenas o
runtime — a parte que realmente precisa de uma stack HTTP assíncrona — usa `tokio` e `reqwest`.

```
tired/
├── crates/
│   ├── tired-syntax/    lexer, parser, AST, diagnósticos, pretty-printer  (sem deps)
│   ├── tired-compiler/  tipos, checker, IR, otimizador                    (sem deps)
│   ├── tired-runtime/   modelo de valores, eval, motores de mock + HTTP, executor, contratos
│   └── tired-cli/       o driver de linha de comando `tired`
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
```

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
