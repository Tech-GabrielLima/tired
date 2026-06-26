"""hale from Python — the Rust compiler + runtime, exposed via PyO3.

    pip install maturin
    maturin develop          # from crates/hale-py/, builds + installs `hale`
    python python/example.py
"""

import hale

print("hale", hale.__version__)

# 1. Type-check a program (the same checks the CLI runs).
bad = 'fetch GitGub /users/x -> u\nlog u.naem'
print("\ncheck(bad) ->")
for msg in hale.check(bad):
    print("  -", msg)

# 2. Infer hale types from a JSON sample.
sample = '{"id": 583231, "login": "octocat", "email": "o@gh.com", "site": "https://x"}'
print("\ninspect ->")
print(hale.inspect(sample, "User"))

# 3. Export a contract as JSON Schema.
print("json_schema ->")
print(hale.json_schema('contract Repo { id: Integer where (> 0)  name: String }', "API"))

# 4. Run a program (executes against real / mocked endpoints).
program = '''
endpoint API { base: "https://api.github.com" timeout: 8s }
fetch API /users/octocat -> u
log "octocat id: {u.id}"
'''
print("\nis_valid(program):", hale.is_valid(program))
print("explain ->")
print(hale.explain(program))
# hale.run(program)   # uncomment to hit the network
