# stonewave

The Stonewave server CLI. It will connect a company's Linux servers to [Wave](https://stonewavetech.com) (module
"Systems"), so the company's dev agent gets safe, scoped hands on them.

> **Prototype.** This first version only shows how the CLI will look: banner, sign-in code, key check, setup
> progress, status and revoke. It changes nothing on the machine and talks to no server.

![connect](media/connect.gif)

```bash
cargo run --release -- connect   # also: status, revoke, banner, --help
```

`NO_COLOR`, pipes, `TERM=dumb` and narrow terminals are handled; the terminal is always restored.

License: Apache-2.0, see [LICENSE](LICENSE). Security: see [SECURITY.md](SECURITY.md).
