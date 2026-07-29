# Piano: controllo e interrogazione dei processi via RPC

> Documento di ingresso. Leggi questo per intero, poi apri solo i file di dettaglio
> del task su cui stai lavorando. I dettagli stanno in `docs/ctl-rpc/`.
>
> Stato: da implementare. Redatto il 2026-07-29.

## Il problema

`monade-cli` consuma questo fork con una sola chiamata: `mprocs::run_mprocs(path).await`
(vedi `../monade-cli/src/start/mod.rs:82` e `:144`). L'obiettivo di monade è permettere a un
agente LLM di **consultare e controllare lo stato dello stack**: sapere quali servizi sono su,
leggerne i log, riavviarne uno.

Oggi non è possibile. Il remote control esiste (`--server` + `--ctl`) ma è **fire and forget**:
in `src/app.rs:57-95` il socket TCP viene letto fino a EOF, deserializzato in un `AppEvent` e
buttato nel canale eventi. Nessuna risposta, nessuna lettura di stato. Tutto lo stato utile
(`State.procs`, `is_up()`, `pid`, buffer vt100) vive dentro il task server e non esce.

Mancano anche due dati che non vengono proprio registrati:

- **l'exit code**: in `src/proc.rs:104` `child.wait()` scarta lo status in `_status`;
- **i log storici**: l'output del pty va solo nel parser vt100, che ha 1000 righe di scrollback
  e non è leggibile dall'esterno.

## Cosa costruiamo

Un **server di controllo request/response su Unix socket**, dentro il processo che già gira.

```
processo `monade start` (uno solo, nessun daemon)
  └─ run_mprocs_with(RunOptions { yaml_path, ctl_socket, log_dir })
       ├─ client_main     TUI crossterm            <── tastiera dell'utente
       ├─ server_main     App::main_loop           ── possiede State { procs }
       │    ├─ ev_rx      AppEvent (keymap, ctl TCP legacy)
       │    ├─ upd_rx     ProcUpdate dai pty
       │    └─ ctl_rx     (CtlRequest, oneshot<CtlResponse>)   <── NUOVO
       └─ ctl_server      UnixListener su ctl_socket           <── NUOVO
              ▲
              │ JSON, una richiesta per riga
              │
        `monade stack ps --json`, `monade stack restart api`, ...
```

Più il **tee dell'output di ogni pty su file**, così i log lunghi si leggono dal filesystem
senza passare dal protocollo.

## Decisioni già prese, non rimetterle in discussione

**Nessun daemon.** Il socket vive dentro il processo `monade start`. Chiudi la TUI, muore
l'albero dei processi e il socket sparisce. È un requisito esplicito: l'utente non vuole
che uccidendo uno stack restino cose in background sulla macchina.

**Si estende questo fork, non si fa re-fork da upstream.** Esiste un `REALIGNMENT_ASSESSMENT.md`
in root che consigliava di ripartire da mprocs 0.9.x (dove upstream sta costruendo `dekit`, un
daemon RPC completo). Quella strada è stata valutata e scartata per questo giro: troppa
riscrittura a monte (~20k righe contro le ~5k del fork, emulatore terminale e pty reimplementati
da zero). Il piano qui sotto è deliberatamente incrementale.

**Il wire protocol però copia la forma di dekit.** Stessi nomi di metodo (`ls`, `start`, `stop`,
`restart`, `kill`, `screen`), stessa busta `{"type":"request","id":N,...}`, stessi codici di
errore. Costa uguale scriverlo così e se un domani si passa a dekit il client in monade-cli non
si tocca. La spec normativa sta in [`docs/ctl-rpc/01-protocol.md`](docs/ctl-rpc/01-protocol.md).

**Retrocompatibilità totale.** `pub async fn run_mprocs(yaml_path: &str)` (`src/lib.rs:44`)
deve continuare a esistere con la stessa firma e lo stesso comportamento. Il binario `mprocs`
e il ctl TCP esistente restano dove sono.

## I task

Da fare in ordine. Ognuno lascia l'albero compilante e testabile.

| # | Task | File di dettaglio | Tocca |
|---|------|-------------------|-------|
| 0 | Spec del protocollo (da leggere prima di 2 e 3) | [`01-protocol.md`](docs/ctl-rpc/01-protocol.md) | niente |
| 1 | Stato dei processi: exit code, started_at, tee su file | [`02-proc-state.md`](docs/ctl-rpc/02-proc-state.md) | `src/proc.rs`, `src/config.rs`, `vendor/pty/src/lib.rs` |
| 2 | Il server sul Unix socket | [`03-ctl-server.md`](docs/ctl-rpc/03-ctl-server.md) | `src/ctl_server.rs` (nuovo), `src/lib.rs` |
| 3 | Aggancio al main loop | [`04-app-wiring.md`](docs/ctl-rpc/04-app-wiring.md) | `src/app.rs` |
| 4 | API pubblica e opzioni | [`05-public-api.md`](docs/ctl-rpc/05-public-api.md) | `src/lib.rs`, `src/config.rs` |
| 5 | Test, verifica manuale, rilascio | [`06-testing-release.md`](docs/ctl-rpc/06-testing-release.md) | `tests/`, `Cargo.toml`, `CHANGELOG.md` |

Il task 1 ha valore anche da solo: appena c'è il tee su file, un LLM può già leggere i log
dei servizi senza che esista alcun protocollo. Se serve un risultato utile in fretta, fermarsi
lì e rilasciare è una tappa legittima.

## Definition of done

Con `monade start` in esecuzione in un terminale, da un altro terminale:

```sh
# elenco con stato, pid, exit code
printf '{"type":"request","id":1,"method":"ls"}\n' | nc -U /tmp/test.sock

# schermata corrente di un servizio
printf '{"type":"request","id":2,"method":"screen","params":{"name":"api"}}\n' | nc -U /tmp/test.sock

# riavvio
printf '{"type":"request","id":3,"method":"restart","params":{"pattern":"api"}}\n' | nc -U /tmp/test.sock
```

e in più:

- la TUI si ridisegna quando un comando RPC cambia lo stato;
- `<log_dir>/api.log` contiene l'output del servizio;
- alla chiusura della TUI il file socket non esiste più;
- `cargo test` passa;
- `run_mprocs(path)` si comporta esattamente come prima.

## Cosa NON fare

- Niente daemon, niente `fork()`, niente `daemonize`. Il processo resta uno.
- Niente riscrittura dell'emulatore terminale, del pty o del layer TUI. Non è questo il giro.
- Non rimuovere né cambiare la firma di `run_mprocs`, `run_app`, il binario `mprocs` o il ctl TCP.
- Niente dipendenze pesanti nuove. Serve solo `serde_json`. In particolare niente `chrono`
  (usa `std::time::SystemTime`) e niente framework RPC.
- Non esporre il socket su TCP. Solo Unix domain socket, permessi `0600`.
- Non aggiungere qui dentro comandi specifici di monade (nginx, stack, domini). Questo crate
  resta generico: sa di processi, non di monade.
