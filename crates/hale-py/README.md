# hale (Python bindings)

Call the hale compiler and runtime from Python. The heavy lifting (lexer, type checker,
optimizer, async runtime) is the same Rust code as the CLI — exposed through
[PyO3](https://pyo3.rs) as an `abi3` extension module, so one wheel works on CPython 3.8+.

## Install / build

```bash
pip install maturin
cd crates/hale-py
maturin develop        # builds the Rust extension and installs `hale` into your venv
# or: maturin build --release   # produces a wheel in target/wheels/
```

## Use

```python
import hale

hale.is_valid(src)            # -> bool: does it type-check?
hale.check(src)               # -> list[str]: diagnostic messages (empty == ok)
hale.explain(src)             # -> str: the parallel plan + request cost
hale.run(src)                 # -> str | None: run the script (raises on error)
hale.inspect(json_text, name) # -> str: infer hale type/contract declarations
hale.json_schema(src, title)  # -> str | None: export types as JSON Schema
```

See [`python/example.py`](python/example.py).

> The functions are thin wrappers over `hale_compiler` / `hale_runtime`. `run` spins a
> Tokio runtime internally, so the same parallel-inference / dedup / dead-request
> elimination apply when you execute a program from Python.
