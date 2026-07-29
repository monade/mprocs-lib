# 06. Test, verifica manuale, rilascio

Task 5. Ultimo passo.

## Stato attuale dei test

`tests/` contiene un solo file, `tests.lua`, residuo del supporto Lua rimosso nel commit
`7a83b58`. Non è un test Rust e non gira. Il crate non ha oggi test di integrazione.

Non serve costruire una suite completa. Servono i test che coprono le cose che si rompono in
silenzio.

## Unit test da scrivere

Nel modulo dove sta il codice, con `#[cfg(test)] mod tests`.

**Il matcher dei pattern** (`src/app.rs`, task 3). È la cosa più facile da sbagliare:

- `matches("api", "api")` vero, `matches("api", "api2")` falso
- `matches("*", qualsiasi)` vero
- `matches("web*", "web")` vero, `matches("web*", "webpack")` vero, `matches("web*", "api")` falso
- `matches("*worker", "sidekiq-worker")` vero, `matches("*worker", "worker-x")` falso

**Il round trip del protocollo** (`src/ctl_server.rs`, task 2), sul modello dei golden test di
dekit (`pvolok/mprocs`, `src/protocol/rpc.rs`, test `golden_methods_encode_exactly`):

una lista di coppie `(CtlRequest, stringa JSON attesa)` verificate in entrambe le direzioni.
Il punto non è testare serde: è che la lista diventa la documentazione eseguibile del wire
format, e un rename accidentale di un campo fa fallire il test invece di rompere `monade-cli`
in produzione.

Aggiungi anche i casi di errore: metodo sconosciuto restituisce `unknown_method`, `params`
di tipo sbagliato restituisce `invalid_params`, campi extra sconosciuti vengono ignorati.

**La sanitizzazione dei nomi di file** (task 1): un processo che si chiama `foo/bar baz` non
deve produrre un path fuori dalla `log_dir`.

## Test di integrazione

Uno solo, ma end to end, in `tests/ctl.rs`:

1. scrivi un `mprocs.yaml` temporaneo in una tempdir con due processi banali
   (`sleep 60` autostart, `echo hi` no autostart);
2. lancia `run_mprocs_with` in un task, con `ctl_socket` e `log_dir` nella tempdir;
3. aspetta che il socket compaia (polling con timeout, non `sleep` fisso);
4. connettiti, manda `ls`, verifica che i due processi ci siano con gli stati attesi;
5. manda `shutdown`, verifica che la funzione ritorni e che il socket sia sparito.

Il problema è che `run_mprocs_with` avvia anche il client TUI, che vuole un terminale vero.
Se questo blocca il test, la via d'uscita pulita è estrarre da `run_client_and_server` una
funzione che avvia server e ctl server **senza** il client, usabile dai test. Non inventare
un finto backend: fai in modo che il codice testabile sia quello vero, con il client fuori.

Se anche questo si rivela costoso, è accettabile fermarsi agli unit test e coprire il resto
con la verifica manuale qui sotto. Non spendere una giornata sul test harness della TUI.

## Verifica manuale

Con un `mprocs.yaml` di prova e il binario `mprocs` compilato in modalità release:

```sh
# terminale 1
cargo run --release -- -c ./mprocs.yaml     # con le opzioni ctl abilitate

# terminale 2
printf '{"type":"request","id":1,"method":"ls"}\n' | nc -U /tmp/mprocs.sock
printf '{"type":"request","id":2,"method":"screen","params":{"name":"api"}}\n' | nc -U /tmp/mprocs.sock
printf '{"type":"request","id":3,"method":"restart","params":{"pattern":"api"}}\n' | nc -U /tmp/mprocs.sock
```

Da controllare a occhio:

- la TUI si ridisegna quando arriva un comando mutante;
- `ls` dopo aver fermato un processo con `x` mostra `exited` e l'exit code giusto;
- i file in `log_dir` contengono l'output e i marker di avvio;
- chiudendo la TUI con `q`, il file socket sparisce;
- `kill -9` sul processo e riavvio: il socket orfano viene ripulito e si riparte.

Prova anche il percorso di **non regressione**: `run_mprocs(path)` senza opzioni deve
comportarsi come la 0.3.0. Il modo più diretto è compilare `monade-cli` contro il crate
locale (`[patch.crates-io]` con un `path`) e lanciare `monade start` su uno stack vero.

## Rilascio

Il processo sta in `RELEASE.md`:

1. `cargo build --release` e `cargo check` puliti
2. changelog e bump di versione in `src/Cargo.toml`
3. commit, tag, push
4. `cargo publish -p monade-mprocs`

Due note su questo repo:

- **Il versionamento è incoerente**: `src/Cargo.toml` dice `0.3.0`, il `CHANGELOG.md` è ancora
  quello di upstream e si ferma a `0.6.4 - 2023-02-17`. Approfittane per riallineare: apri una
  sezione `## 0.4.0` in cima con le voci di questo lavoro e, se vuoi, una nota che spiega che
  la numerazione del fork è indipendente da quella di mprocs.
- La versione da pubblicare è **0.4.0**: è additiva (nessuna rottura di API pubblica) ma
  aggiunge parecchia superficie.

Voci di changelog da scrivere:

```
## 0.4.0

- Add `run_mprocs_with(RunOptions)` with optional control socket and per-process log files
- Add JSON control server over a Unix socket: `ls`, `screen`, `start`, `stop`, `restart`,
  `kill`, `shutdown`
- Track process exit code, terminating signal and start time
- Tee process output to `<log_dir>/<name>.log` when configured
- `run_mprocs(path)` is unchanged and keeps its behaviour
```

Dopo la pubblicazione, in `monade-cli` basta bumpare la versione in `Cargo.toml` e fare le
modifiche descritte in [`05-public-api.md`](05-public-api.md#modifiche-lato-monade-cli-per-contesto-non-farle-qui).
