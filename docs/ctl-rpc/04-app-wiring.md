# 04. Aggancio al main loop

Task 3. Tocca solo `src/app.rs`. È il pezzo che legge davvero lo stato.

## Perché passa dal main loop

`State` (`src/state.rs:33`) è posseduto da `App`, che vive dentro il task `server_main`
(`src/app.rs:863`). Non è dietro un `Arc<Mutex>` e non deve diventarlo: renderlo condiviso
significherebbe poter osservare stati intermedi mentre il loop sta mutando i processi.

`main_loop` (`src/app.rs:110`) è un `select!` su tre canali con `&mut self`. Aggiungere un
quarto ramo dà accesso diretto e sincronizzato a `self.state`, senza lock e senza race. È il
motivo per cui questo piano è piccolo.

Snapshot condivisi aggiornati a ogni render sono l'alternativa sbagliata: sarebbero stantii e
non permetterebbero comunque i comandi mutanti. Non farlo.

## Modifiche a `App`

Aggiungi il campo (`src/app.rs:40`):

```rust
ctl_rx: UnboundedReceiver<CtlMessage>,
```

`server_main` (`src/app.rs:863`) crea il canale insieme agli altri due
(`src/app.rs:885-889` è dove nascono `upd_tx`/`ev_tx`), tiene il `Sender` e lo restituisce
o lo passa al chiamante, che lo consegna a `ctl_server_main`. Il modo più pulito è che
`server_main` accetti il `Receiver` già costruito come parametro, così è `run_client_and_server`
in `src/lib.rs:150` a creare il canale e ad avviare entrambi i task. Vedi
[`05-public-api.md`](05-public-api.md).

Quando `ctl_socket` non è configurato, il canale esiste comunque ma nessuno ci scrive: il ramo
del `select!` resta inerte. Non serve rendere il campo opzionale.

## Il quarto ramo

Nel `select!` a `src/app.rs:162-182`, accanto a `client_rx`, `upd_rx` e `ev_rx`:

```rust
msg = self.ctl_rx.recv().fuse() => {
  if let Some((req, reply)) = msg {
    let (response, action) = self.handle_ctl(req);
    let _ = reply.send(response);
    action
  } else {
    LoopAction::Skip
  }
}
```

L'errore sul `reply.send` si ignora: significa solo che il client ha chiuso la connessione
prima della risposta.

`handle_ctl` restituisce anche un `LoopAction` perché i comandi mutanti devono far ridisegnare
la TUI. Restituisci `LoopAction::Render` per `start`/`stop`/`restart`/`kill`, `Skip` per `ls`
e `screen`. Per `shutdown`, replica quello che fa `AppEvent::Quit` in `handle_event`
(`src/app.rs:485` circa): manda la risposta **prima** di avviare la chiusura, altrimenti il
client non la riceve mai.

## `handle_ctl`

Un metodo su `App`, `fn handle_ctl(&mut self, req: CtlRequest) -> (CtlResponse, LoopAction)`.

### Risoluzione del pattern

Una funzione libera nel modulo:

```rust
fn matches(pattern: &str, name: &str) -> bool
```

Nome esatto, oppure un solo `*` interpretato come prefisso, suffisso o "tutto". Niente regex.
Tienila fuori da `App` così è testabile da sola: è la cosa più facile da sbagliare in questo
task e merita i suoi unit test.

### `ls`

Itera `self.state.procs` filtrando sul pattern, e costruisci per ognuno l'oggetto descritto in
[`01-protocol.md`](01-protocol.md#ls).

La mappatura di `state` viene da `Proc.inst` (`src/proc.rs:160-165`) e da `Proc::is_up()`
(`src/proc.rs:246`). Attenzione a distinguere `idle` da `exited`: entrambi hanno `is_up() ==
false`, ma `idle` è `ProcState::None` mentre `exited` è `ProcState::Some(inst)` con
`inst.running == false`. Non usare `is_up()` da solo, fai il match su `inst`.

`pid` va letto da `inst.pid` e incluso **solo** se lo stato è `running`: un pid di un processo
morto è un'informazione trappola per chi legge.

`exit_code` e `signal` vengono dai campi aggiunti nel task 1.

### `screen`

Cerca il processo per nome esatto. Se non c'è, `no_match`.
Se c'è, `proc.lock_vt()` (`src/proc.rs:254`) restituisce `Option<RwLockReadGuard<vt100::Parser>>`:
`None` significa che non c'è istanza, e la risposta è `{"screen": null}`.
Altrimenti `vt.screen().contents()`, che dà già testo semplice senza ANSI.

Usa `lock_vt()`, **non** `lock_vt_mut()`. La versione mutabile serve a chi manipola l'offset di
scrollback e qui non serve.

### `start`, `stop`, `restart`, `kill`

Attenzione: gli handler esistenti in `src/app.rs:542-570` agiscono su
`get_current_proc_mut()`, cioè sul processo **selezionato nella TUI**. Non riusarli e non
passare per `AppEvent`: cambierebbero il processo sbagliato.

Itera i processi che corrispondono al pattern e chiama direttamente il metodo su ognuno,
replicando la semantica degli handler:

- `start` -> `proc.start()`
- `stop` -> `proc.stop()`
- `kill` -> `proc.kill()`
- `restart` -> se `proc.is_up()` allora `proc.stop()` e `proc.to_restart = true`, altrimenti
  `proc.start()`. Il riavvio effettivo lo fa `handle_proc_update` quando arriva
  `ProcUpdate::Stopped` (`src/app.rs:794-799`), non farlo qui.

Rispondi `{"matched": n}` dove `n` è il numero di processi toccati. Zero non è un errore.

Non aspettare che i processi cambiano stato: la risposta è immediata per contratto.

## Criteri di accettazione

- Con lo stack in esecuzione, `ls` via `nc -U` restituisce nomi, stati e pid coerenti con la TUI.
- Fermare un servizio dalla TUI con `x` e poi chiamare `ls` mostra `exited` con l'exit code giusto.
- `restart` via RPC riavvia il servizio e la TUI si ridisegna da sola.
- `start` su un pattern che non corrisponde a nulla risponde `{"matched":0}`, non un errore.
- `screen` su un nome inesistente risponde `no_match`.
- Unit test su `matches()` per: nome esatto, `*`, prefisso, suffisso, non corrispondenza.
