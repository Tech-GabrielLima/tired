# tired (Python bindings)

Call the TIRED compiler and runtime from Python. The heavy lifting (lexer, type checker,
optimizer, async runtime) is the same Rust code as the CLI — exposed through
[PyO3](https://pyo3.rs) as an `abi3` extension module, so one wheel works on CPython 3.8+.

## Install / build

```bash
pip install maturin
cd crates/tired-py
maturin develop        # builds the Rust extension and installs `tired` into your venv
# or: maturin build --release   # produces a wheel in target/wheels/
```

## Use

```python
import tired

tired.is_valid(src)            # -> bool: does it type-check?
tired.check(src)               # -> list[str]: diagnostic messages (empty == ok)
tired.explain(src)             # -> str: the parallel plan + request cost
tired.run(src)                 # -> str | None: run the script (raises on error)
tired.inspect(json_text, name) # -> str: infer TIRED type/contract declarations
tired.json_schema(src, title)  # -> str | None: export types as JSON Schema
```

See [`python/example.py`](python/example.py).

> The functions are thin wrappers over `tired_compiler` / `tired_runtime`. `run` spins a
> Tokio runtime internally, so the same parallel-inference / dedup / dead-request
> elimination apply when you execute a program from Python.
