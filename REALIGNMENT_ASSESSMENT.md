# Valutazione di fattibilità: riallineamento del fork a mprocs upstream

> Analisi dei repository `mprocs-lib` (questo fork), `../mprocs` (upstream 0.9.4) e
> `../monade-cli` (consumatore della libreria). Data: 2026-05-30.

## Verdetto in breve

**Il riallineamento è fattibile, e la premessa che aveva bloccato lo sviluppo è in realtà falsa.**
Il nuovo modello client/server di mprocs **non** richiede processi separati per questo caso d'uso:
nel percorso di default gira tutto **in-process**. Quindi `monade-cli` può continuare a chiamare
una funzione `run_mprocs(path)` esattamente come ora.

Il costo vero del riallineamento non è il client/server — è il fatto che a monte hanno
**riscritto da zero gli strati bassi** (~20k righe vs ~5k del fork).

## Il punto chiave: l'esecuzione di default è in-process

Nel nuovo mprocs, `mprocs_main()` → `run_app()`, nel ramo normale (senza `--server`), fa questo
(`src/mprocs/mprocs.rs:207-260`):

```rust
// pipe in-memoria, NON socket/processi separati
let (srv_to_clt_sender, srv_to_clt_receiver) = tokio::io::simplex(8*1024) ...
let (clt_to_srv_sender, clt_to_srv_receiver) = tokio::io::simplex(8*1024) ...

let kernel = Kernel::new();                          // kernel in-process
let app_task_id = create_app_task(config, keymap, &pc);
tokio::spawn(client_loop(...));                      // client come task tokio
tokio::spawn(kernel.run());                          // server come task tokio
client_main(clt_to_srv_sender, srv_to_clt_receiver).await  // TUI
```

`daemonize` viene usato **solo** da `dekit` e `daemon/spawn.rs` (nessun riferimento dal flusso TUI
principale). Il fork attuale fa la stessa identica cosa, solo con canali `tokio::mpsc` invece dei
`simplex`. Concettualmente **è la stessa architettura** — `run_client_and_server` (`lib.rs:150`)
corrisponde uno-a-uno al ramo `None =>` del nuovo `run_app`.

Esporre la libreria è quindi banale: si fattorizza quel blocco di bootstrap in una funzione
`run_mprocs(config_path)` e si espone in `lib.rs`. La superficie che `monade-cli` usa è **una sola
funzione** (`use mprocs::run_mprocs; run_mprocs(path).await`).

## Cos'è cambiato davvero a monte (il costo reale)

Il fork è basato su mprocs ~0.6.4, proprio quando upstream *iniziava* il client/server
(`36471eb Start moving to client-server architecture`). Da allora hanno riscritto quasi tutto.
Upstream è ora 0.9.4:

| Strato | Fork (0.3.0) | Nuovo mprocs (0.9.4) |
|---|---|---|
| Edition / clap | 2021 / clap 3 | 2024 / clap 4 |
| Emulatore terminale | `vendor/vt100` (mprocs-vt100) | `src/term/` proprietario (parser, screen, grid, cell) |
| Driver terminale | crossterm 0.23 + tui 0.18 | `src/term_driver/` proprietario |
| PTY | `vendor/pty` (portable-pty) | `src/process/` proprietario (rustix) |
| Scheduling | semplice | `src/kernel/` (task system, path_trie) |
| Config | YAML (+ lua rimosso) | YAML + JSON + **Lua (mlua)** + **JS (rquickjs)** |
| Remote control | `ctl.rs` minimale | `dekit` (daemon RPC completo) |

**Sono stati eliminati i crate vendored** (`vt100`, `portable-pty`, crossterm, tui) reimplementandoli
internamente. Per questo non è possibile un semplice `git merge`: i due alberi non condividono più
quasi nessun file.

## Le personalizzazioni del fork sono piccole (buona notizia)

Dal git log, sopra la base upstream è stato aggiunto pochissimo:

- `372a96e` la funzione libreria `run_mprocs` + lib name → **da riportare** (banale)
- `7a83b58` rimozione lua → ora **superato**: upstream ha lua+js, da decidere se tenerli
- `4f0efd0` fix window resize → probabilmente **già risolto** a monte
- `5777a0b` clear buffer con tasto 'w' → **da riportare** (piccola modifica UI, verificare se esiste già)
- flag build aarch64 darwin, release process, package info → **da riportare** (config, non codice)

In pratica il "delta" funzionale è ~1 funzione di ingresso + 1 keybind.

## Strategia consigliata

Non è "adattare il vecchio fork" ma **re-fork da upstream 0.9.4 e ri-applicare il delta**:

1. Ripartire dall'attuale `../mprocs` (0.9.4) come nuova base del fork.
2. Aggiungere `pub fn run_mprocs(config_path)` fattorizzando il ramo in-process di `run_app`
   (niente parsing argv, accetta direttamente il path).
3. Esporlo in `lib.rs` mantenendo il nome crate `mprocs`/`monade-mprocs` per non rompere
   `monade-cli` (che fa `use mprocs::run_mprocs`).
4. Ri-applicare il keybind 'w' (verificare prima se c'è già).
5. Riportare i flag build aarch64 + il processo di release, bumpare a 0.4.0, ripubblicare su crates.io.
6. In `monade-cli` cambiare solo la versione in `Cargo.toml` (l'API resta `run_mprocs(path).await`).

**Stima**: il grosso del lavoro è verificare/sistemare build e pubblicazione del nuovo albero
(edition 2024, clap 4, dipendenze nuove come rquickjs/mlua/rustix), non scrivere codice.
Ragionevolmente fattibile in 1-2 sessioni focalizzate.

## Cose interessanti che si portano dietro "gratis"

Oltre al server (e proprio in ottica MCP):

- **`dekit`**: daemon RPC già pronto con `DkRequest` = `Spawn/Ls/Start/Stop/Kill/Restart/Screen` e
  `DkResponse::Screen` (dump del buffer). È **letteralmente la base per un MCP di mprocs** — basta
  esporre quegli RPC come tool.
- **Config JS e Lua** per generare i processi dinamicamente (si potrebbe rimpiazzare il "compile
  YAML → mprocs.yaml temporaneo" con un config programmatico).
- **Procfile, justfile, npm** come sorgenti di processi.
- **Logging su file** per-processo (`--log-dir`, `--log-file`, `{name}/{pid}/{ts}`), **dipendenze
  tra processi** (`deps`), **autorestart**, hook **`on-init`/`on-all-finished`**.

## Rischi / caveat

- `UnixProcessesWaiter::init()` installa gestione globale di SIGCHLD: dentro `monade-cli` (che ha
  già un suo runtime/segnali) va verificato che non confligga. Il fork attuale non ha questo, quindi
  è un comportamento nuovo da testare.
- La nuova base trascina dipendenze pesanti (mlua *vendored*, rquickjs): tempi di build e dimensione
  binario aumentano. Se non servono, valutare feature-gate per lua/js.
- `panic = "abort"` + `daemonize` su unix sono nelle dipendenze: assicurarsi che la build come
  libreria non tiri dentro il path daemon in modo problematico (non dovrebbe, è opzionale).

## Prossimo passo proposto

Proof-of-concept concreto: partire da `../mprocs`, aggiungere `run_mprocs(path)` e provare a
compilarlo come libreria, così la valutazione diventa verificata. Da fare in un worktree isolato per
non toccare `../mprocs`.
