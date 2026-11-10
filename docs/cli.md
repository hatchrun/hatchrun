# Command line

`hatchrun` is a static binary, released as
`hatchrun-<version>-linux-<arch>.tar.gz` for `amd64` and `arm64`, with
a `SHA256SUMS` file and signed build provenance. It has two commands,
both of which run locally and make no network calls.

```sh
hatchrun --version
hatchrun --help
```

## validate

```sh
hatchrun validate [PATH]
```

`PATH` defaults to `.hatchrun/config.yml`. The command parses the file
and runs every validation rule, printing each problem on its own line:

```text
.hatchrun/config.yml:12:7: unknown executor `ubuntu-24.04`
```

The position is omitted when the problem does not belong to one line:

```text
.hatchrun/config.yml: `jobs.build` needs a `runs-on` or an `executor`
```

- Exit code `0`: the file is valid.
- Exit code `1`: at least one problem was printed, or the file could not
  be read.

A file over 1 MiB is refused without being loaded. The same limit
applies to each file `convert` reads.

## convert

```sh
hatchrun convert [FILES]... [-o OUTPUT]
```

Converts GitHub Actions, Forgejo Actions or Gitea Actions workflow files
into one hatchrun config. With no `FILES`, it reads `*.yml` and `*.yaml`
from each of these directories that exists:

1. `.github/workflows`
2. `.forgejo/workflows`
3. `.gitea/workflows`

The config is written to stdout, or to `OUTPUT` with `-o`/`--output`.
The parent directory of `OUTPUT` is created when missing, so
`hatchrun convert -o .hatchrun/config.yml` works in a fresh checkout.

Problems go to stderr, prefixed with `warning:` or `error:` and naming
the source file:

```text
warning: .github/workflows/ci.yml: jobs.build: unknown key `defaults` was dropped
error: .github/workflows/deploy.yml: jobs.deploy: `environment` must be a name or a map with `name`
```

- A `warning:` is something dropped without losing a job, such as an
  unsupported step option. It leaves the exit code at `0`.
- An `error:` is a job or a file left out of the config. Any error, or a
  file that could not be read or was too large, makes the exit code `1`.
- Two inputs with the same file name are refused, because problems are
  reported by name; convert them in separate runs.
- When none of the default directories exists, the command fails and
  asks for the files to be named.

Typical use:

```sh
hatchrun convert -o .hatchrun/config.yml
hatchrun validate .hatchrun/config.yml
```

Review the generated file before committing it: the converter keeps the
jobs it can express, and reports the rest rather than guessing.
