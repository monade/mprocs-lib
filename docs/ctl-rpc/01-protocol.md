# 01. Spec del protocollo di controllo

Documento **normativo**. I task 2 e 3 implementano questa spec, il client in `monade-cli` la
consuma. Se qualcosa qui è ambiguo, risolvi guardando come lo fa dekit
(`pvolok/mprocs`, `src/protocol/ctl.rs` e `src/protocol/rpc.rs` su master) e allineati a quello.

## Trasporto

Unix domain socket, `SOCK_STREAM`, path passato in `RunOptions.ctl_socket`.
Permessi del file `0600`. Mai TCP.

Una connessione può portare più richieste in sequenza. Il server risponde nell'ordine di
arrivo. Il client può anche aprire e chiudere una connessione per ogni richiesta, ed è quello
che farà `monade-cli`.

## Framing

JSON UTF-8, **un oggetto per riga**, terminato da `\n`. `serde_json::to_string` in forma
compatta non emette mai newline dentro il valore, quindi il framing regge senza escaping.

Righe vuote: ignorate. Riga non parsabile: il server risponde con un errore `invalid_params`
e `id: 0`, poi chiude la connessione.

## Busta dei messaggi

Appena accettata la connessione, il server manda **una** riga di hello e poi resta in ascolto:

```json
{"type":"hello","protocol":1,"app":"monade-mprocs 0.4.0","features":[]}
```

Il client può ignorarla. Serve a rendere il protocollo diagnosticabile a mano e compatibile
con la forma di dekit.

Richiesta:

```json
{"type":"request","id":1,"method":"ls","params":{}}
```

`params` può essere assente o `null` se il metodo non prende argomenti.

Risposta, successo:

```json
{"type":"response","id":1,"result":{"procs":[]}}
```

Risposta, errore:

```json
{"type":"response","id":1,"error":{"code":"no_match","message":"no proc matches 'api'"}}
```

`result` ed `error` sono mutuamente esclusivi. `id` è quello della richiesta.

## Codici di errore

Sono API. Non rinominarli, non riusarli per altro.

| Codice | Quando |
|---|---|
| `unknown_method` | `method` non riconosciuto |
| `invalid_params` | JSON malformato, o `params` non conforme al metodo |
| `no_match` | il metodo richiede un target e nessun processo corrisponde |
| `internal` | qualsiasi errore inatteso lato server |

Nota: per i verbi che agiscono su un pattern (`start`, `stop`, `restart`, `kill`), zero
corrispondenze **non è un errore**: si risponde `{"matched": 0}` e sta al client decidere se
è un problema. `no_match` è riservato ai metodi che puntano a un singolo processo per nome
(`screen`).

## Selettori

I verbi che agiscono su più processi prendono `params.pattern`, una stringa:

- nome esatto: `api`
- glob con un solo `*`: `web*`, `*worker`, `*` (tutti)

Il match è case sensitive sul nome del processo. Niente regex, niente tag, niente path
gerarchici: il modello di questo fork è una lista piatta di nomi.

I metodi che puntano a un processo singolo prendono `params.name`, che deve essere un nome
esatto.

## Metodi

### `ls`

`params`: `{"pattern": "..."}` opzionale. Senza pattern, tutti i processi.

```json
{"type":"response","id":1,"result":{"procs":[
  {"name":"api","id":1,"state":"running","pid":54321,"started_at":1753790000,
   "log_file":"/Users/x/.config/monade/logs/monade-cli/api.log"},
  {"name":"worker","id":2,"state":"exited","exit_code":1,"signal":null,
   "started_at":1753789000,"log_file":"..."},
  {"name":"mailhog","id":3,"state":"idle","log_file":null}
]}}
```

Campi sempre presenti: `name`, `id`, `state`.
Campi presenti quando ha senso: `pid` (solo se `running`), `exit_code` e `signal` (solo se
`exited`), `message` (solo se `error`), `started_at` (epoch secondi, ultimo avvio),
`log_file` (path assoluto, `null` se il logging su file è disattivo).

Valori di `state`, mappati da `ProcState` (`src/proc.rs:160-165`):

| `state` | Da cosa deriva |
|---|---|
| `idle` | `ProcState::None`: mai avviato, o fermato e ripulito |
| `running` | `ProcState::Some(inst)` con `inst.running == true` |
| `exited` | `ProcState::Some(inst)` con `inst.running == false` |
| `error` | `ProcState::Error(msg)`: lo spawn è fallito |

I token sono gli stessi di dekit dove il concetto coincide. Non inventarne altri.

### `screen`

`params`: `{"name": "api"}`, obbligatorio.

Restituisce **la schermata visibile** del processo, testo semplice senza sequenze ANSI, presa
da `vt100::Screen::contents()` tramite `Proc::lock_vt()` (`src/proc.rs:254`).

```json
{"type":"response","id":2,"result":{"screen":"Listening on 0.0.0.0:3000\n..."}}
```

Se il processo esiste ma non ha una istanza viva, `{"screen": null}`.
Se il nome non esiste, errore `no_match`.

**Non** implementare qui la lettura dello scrollback. Per la cronologia lunga c'è il file in
`log_file`: il client lo legge dal filesystem. Grattare lo scrollback vt100 richiederebbe di
muovere l'offset di vista, che è lo stesso stato che sta guardando l'utente nella TUI.

### `start`, `stop`, `restart`, `kill`

`params`: `{"pattern": "..."}`, obbligatorio.

```json
{"type":"response","id":3,"result":{"matched":1}}
```

Semantica, allineata a quello che già fanno gli handler in `src/app.rs:542-570`:

- `start`: su ogni processo che corrisponde e non è su, `Proc::start()`.
- `stop`: `Proc::stop()`, che rispetta lo `StopSignal` configurato (`SIGTERM` di default,
  oppure `send-keys` se il monade.yaml lo definisce).
- `kill`: `Proc::kill()`, hard kill immediato.
- `restart`: se è su, `Proc::stop()` e `to_restart = true` (il riavvio lo fa
  `handle_proc_update` a `src/app.rs:794-799`); se è giù, `Proc::start()`.

`matched` conta i processi su cui si è agito, non quelli che hanno effettivamente cambiato
stato. **La risposta torna subito**: fermare un processo è asincrono. Il client che vuole
conferma fa polling di `ls`. Documentalo, è la fonte di bug più probabile lato client.

### `shutdown`

`params`: nessuno. Equivale a `AppEvent::Quit`: ferma tutti i processi e chiude l'app.
Risponde `{}` prima di iniziare la chiusura.

## Compatibilità

`protocol: 1`. Si bumpa solo se cambia il framing o la busta. Aggiungere metodi, campi o
codici di errore è additivo e **non** bumpa la versione: i client devono ignorare i campi
che non conoscono.
