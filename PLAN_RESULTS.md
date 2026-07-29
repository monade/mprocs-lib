# Risultati: controllo e interrogazione dei processi via RPC

Esecuzione di [`PLAN.md`](PLAN.md). Tutti e 5 i task fatti, in ordine, un commit
ciascuno. Niente version bump, niente push, niente publish.

| Task | Commit | Stato |
|---|---|---|
| 1 — exit code, started_at, tee su file | `6c58b7c` | fatto |
| 2 — server sul Unix socket | `7554abe` | fatto |
| 3 — aggancio al main loop | `b0dfe86` | fatto |
| 4 — API pubblica e opzioni | `b0dfe86` | fatto |
| 5 — test e documentazione | `139a742` | fatto |

`cargo check` pulito, nessun warning nuovo (erano 22, ora 14: ho tolto tre
import morti in `lib.rs`). `cargo test` verde: 26 test.

> I due test `attrs` e `colors` di `mprocs-vt100` falliscono, ma fallivano già
> prima di questo lavoro — verificato con `git stash`. Sono nel vendored vt100,
> che non ho toccato.

## L'interfaccia

### Per chi consuma il crate (monade-cli)

`run_mprocs(&str)` è **invariata**, stessa firma e stesso comportamento: senza
socket e senza log su file. Le capacità nuove stanno in una funzione affiancata.

```rust
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
  pub yaml_path: PathBuf,
  pub ctl_socket: Option<PathBuf>,   // apre il socket di controllo
  pub log_dir: Option<PathBuf>,      // <log_dir>/<nome>.log per processo
  pub log_max_bytes: Option<u64>,    // default 8 MiB
}

pub async fn run_mprocs_with(opts: RunOptions) -> anyhow::Result<()>;
```

Costruibile con `..Default::default()`, così aggiungere opzioni in futuro non
rompe i chiamanti.

Se il socket è occupato da un'istanza viva, `run_mprocs_with` **fallisce
subito** con un errore leggibile invece di degradare in silenzio.

### Il protocollo

Unix domain socket, `0600`, mai TCP. JSON, un oggetto per riga. Il socket vive
dentro il processo: chiudi mprocs e sparisce, nessun daemon.

```
{"type":"hello","protocol":1,"app":"monade-mprocs 0.3.0","features":[]}
{"type":"request","id":1,"method":"ls","params":{}}
{"type":"response","id":1,"result":{"procs":[...]}}
{"type":"response","id":1,"error":{"code":"no_match","message":"..."}}
```

| Metodo | Params | Result |
|---|---|---|
| `ls` | `pattern` (opzionale) | `{"procs":[...]}` |
| `screen` | `name` (esatto) | `{"screen":"..."}` o `{"screen":null}` |
| `start` `stop` `restart` `kill` | `pattern` | `{"matched":n}` |
| `shutdown` | — | `{}` |

Codici di errore: `unknown_method`, `invalid_params`, `no_match`, `internal`.

Voce di `ls`: `name`, `id`, `state` sempre presenti; `pid` solo se `running`
(il pid di un processo morto è un'informazione trappola); `exit_code` e
`signal` solo se `exited`; `message` solo se `error`; `started_at` (epoch
secondi) e `log_file` quando hanno senso. `state` ∈ `idle | running | exited |
error`.

`pattern` è un nome esatto oppure un solo `*` come prefisso, suffisso o
"tutto": `api`, `web*`, `*worker`, `*`. Niente regex.

**Le risposte di `start`/`stop`/`restart`/`kill` tornano subito.** Fermare un
processo è asincrono: `matched` conta i processi su cui si è agito, non quelli
che hanno cambiato stato. Il client che vuole conferma fa polling di `ls`.
È la fonte di bug più probabile lato client.

### Dal binario

Due argomenti nuovi, additivi:

```sh
mprocs -c mprocs.yaml --ctl-socket /tmp/mprocs.sock --log-dir /tmp/mprocs-logs
```

Non erano nel piano: li ho aggiunti perché la verifica manuale descritta nel
doc 06 altrimenti non è eseguibile. Confermato con te prima di scriverli.

## Com'è fatto dentro

Il pezzo che conta: le richieste passano da un **quarto ramo del `select!`**
del main loop (`src/app.rs`), non da uno stato condiviso. `State` resta
posseduto da `App`, senza `Arc<Mutex>`: le richieste vedono esattamente lo
stato che la TUI sta disegnando, senza lock e senza race. È il motivo per cui
la modifica è piccola.

- `src/ctl_server.rs` (nuovo, ~460 righe + test) — solo I/O e serializzazione.
  Non tocca lo stato: traduce righe JSON in `CtlRequest`, le manda al main loop
  e riscrive la risposta. Hello alla connessione, un task per connessione,
  timeout di 5s sulla risposta, socket orfano ripulito, guard con `Drop` che
  rimuove il file su ogni via d'uscita.
- `src/app.rs` — `handle_ctl`, `matches()`, `describe_proc()`. I verbi iterano
  i processi che corrispondono al pattern e chiamano il metodo su ognuno: gli
  handler esistenti agiscono sul processo *selezionato nella TUI* e riusarli
  avrebbe toccato quello sbagliato.
- `src/proc.rs` — `ProcUpdate::Stopped { exit_code, signal }`, `Inst.started_at`,
  tee dell'output del pty su file.
- `src/config.rs` — `ctl_socket`, `log_dir`, `log_max_bytes`, popolati dal
  codice e **mai** dal YAML: sono decisioni dell'host, non dello stack.
- `vendor/pty/src/lib.rs` — accessore `ExitStatus::signal()`.

I log sono **bytes grezzi, ANSI incluso**: lo strip lo fa il consumatore.
Apertura in append, marker `=== mprocs: <nome> started, pid=..., at=... ===` a
ogni avvio, troncamento se il file supera il tetto. Il nome file è sanitizzato
(`weird name/with slash` → `weird_name_with_slash.log`), quindi un nome scritto
da un umano non può uscire dalla `log_dir`.

Unica dipendenza nuova: `serde_json`, già nel `Cargo.lock`.

## Test

26 test, tutti in-crate.

- `matches()`: nome esatto, `*`, prefisso, suffisso, non corrispondenza.
- Round trip golden del wire format: coppie `(CtlRequest, JSON atteso)`
  verificate nelle due direzioni. Un rename accidentale di un campo fa fallire
  il test invece di rompere `monade-cli` in produzione. Più i casi di errore:
  metodo sconosciuto, params sbagliati, campi extra ignorati, riga malformata.
- Sanitizzazione dei nomi: `..`, `../../etc/passwd`, `/` non escono dalla dir.
- Tetto dei log: append sotto la soglia, troncamento sopra, e un file non
  apribile non fa cadere il processo.
- **End to end** (`src/ctl_test.rs`): scrive un `mprocs.yaml` temporaneo, avvia
  il vero `server_main` + `ctl_server_main`, si connette al socket e verifica
  hello, `ls`, pattern, `screen` (esistente, inesistente, senza istanza),
  `start`, exit code dopo la morte del processo, metodo sconosciuto, contenuto
  del file di log, `shutdown` e sparizione del socket. Più un test su bind con
  socket orfano vs socket vivo.

Il test e2e sta dentro il crate e non in `tests/` perché `run_mprocs_with`
avvia anche il client TUI, che vuole un terminale vero. Il finto client qui è
solo una coppia di canali — è esattamente ciò che il client vero fa dal lato
server — quindi il codice sotto test è quello che poi gira davvero, senza
aggiungere una funzione headless all'API pubblica.

## Verifica manuale

Fatta col binario release su un pty, non solo dai test. Tutti i punti della
definition of done del piano:

- `ls` con nomi, stati e pid coerenti; `screen` che restituisce il testo;
  `restart` che cambia pid e `started_at`;
- la TUI si ridisegna da sola quando arriva un comando mutante (verificato
  contando i byte scritti sul pty prima e dopo);
- `<log_dir>/api.log` con l'output e un marker per esecuzione;
- `stop` che rispetta lo `StopSignal` configurato (`gentle` con `SIGINT` esce
  con `exit_code: 0`, `signal: null`);
- exit code corretto (`crasher` → `exit_code: 3`) e segnale valorizzato dopo
  uno `stop` (`signal: "Terminated: 15"`);
- alla chiusura il file socket non esiste più;
- `kill -9` sul processo e riavvio: il socket orfano viene ripulito;
- due istanze sullo stesso path: la seconda rifiuta di partire con
  `Control socket ... is already in use by another running instance.`;
- non regressione: senza i flag nuovi non si crea nessun socket e nessun log.

`examples/mprocs.yaml` è il playground usato per queste prove, con dentro i
comandi da copiare e incollare.

## Due cose da sapere

**Path del socket corti.** macOS limita `sun_path` a ~104 byte: un path lungo
fa fallire il bind. L'errore ora riporta la causa nel messaggio, non solo nella
source chain, perché i chiamanti stampano `{}`.

**Un processo che ignora SIGTERM blocca la chiusura.** mprocs aspetta che tutti
i processi siano giù prima di uscire, quindi `q` e `shutdown` restano appesi.
È comportamento preesistente di mprocs, non introdotto dal socket, ma il socket
lo rende più facile da incontrare: `kill` sul pattern è la via d'uscita.
`examples/mprocs.yaml` ha un processo `stubborn` apposta, con l'avvertenza.

## Cosa non ho fatto, di proposito

- **Nessun version bump** e nessun `cargo publish`: le voci di changelog stanno
  sotto `## Unreleased`, la versione la decidi tu. `RELEASE.md` ha il processo.
- Nessun daemon, nessun `fork()`. Il processo resta uno.
- Nessuna lettura dello scrollback via protocollo: per la cronologia lunga c'è
  il file in `log_file`, come da spec.
- Nessun comando specifico di monade qui dentro. Il crate sa di processi, non
  di monade.
- Le modifiche lato `monade-cli` descritte nel doc 05 stanno in quel repo.
