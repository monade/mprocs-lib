# 03. Il server sul Unix socket

Task 2. File nuovo `src/ctl_server.rs`, più la dichiarazione del modulo in `src/lib.rs`.
Leggi prima [`01-protocol.md`](01-protocol.md).

Questo modulo fa solo I/O e serializzazione. **Non tocca lo stato**: traduce righe JSON in
`CtlRequest`, le manda al main loop, aspetta la risposta su un oneshot, la riscrive sul socket.
La logica che legge `State` sta nel task 3.

## Dipendenza da aggiungere

In `src/Cargo.toml`:

```toml
serde_json = "1.0"
```

È l'unica dipendenza nuova di tutto il piano. È già nel `Cargo.lock` per via di altri crate,
quindi non muove l'albero.

## Tipi

Definiscili qui, con `Serialize`/`Deserialize`. Rispecchiano la spec uno a uno.

```rust
#[derive(Debug, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum CtlRequest {
  Ls { #[serde(default)] pattern: Option<String> },
  Screen { name: String },
  Start { pattern: String },
  Stop { pattern: String },
  Restart { pattern: String },
  Kill { pattern: String },
  Shutdown,
}
```

Non deserializzare direttamente la busta in questo enum: `method` e `params` stanno nella
busta esterna, che ha anche `type` e `id`. Fai come dekit
(`src/protocol/rpc.rs`, funzione `from_wire`): leggi la busta in una struct con
`method: String` e `params: serde_json::Value`, poi ricomponi un `Value` con quei due campi e
passalo a `serde_json::from_value` per ottenere il `CtlRequest`.

Il motivo è la qualità degli errori: se il metodo non è in una lista nota rispondi
`unknown_method`, altrimenti un fallimento di parsing è `invalid_params`. Senza questo passaggio
ogni errore diventa indistinguibile.

Per la risposta:

```rust
pub enum CtlResponse {
  Ok(serde_json::Value),
  Err { code: &'static str, message: String },
}
```

## Il listener

Firma suggerita:

```rust
pub async fn ctl_server_main(
  socket_path: PathBuf,
  tx: tokio::sync::mpsc::UnboundedSender<CtlMessage>,
  shutdown: triggered::Listener,
) -> anyhow::Result<()>
```

dove `CtlMessage = (CtlRequest, tokio::sync::oneshot::Sender<CtlResponse>)`.

`triggered` è già una dipendenza e viene già usata così in `App::run` (`src/app.rs:54-56`,
`exit_trigger`/`exit_listener`): riusa lo stesso meccanismo per fermare il listener.

Struttura del loop, modellata su quella del ctl TCP esistente in `src/app.rs:63-94`:

```
loop {
  select! {
    _ = shutdown.clone() => break,
    conn = listener.accept() => { tokio::spawn(handle_conn(stream, tx.clone())); }
  }
}
```

## Bind e socket stale

Un `UnixListener::bind` fallisce se il path esiste già. Prima di bindare:

1. se il file non esiste, binda e vai;
2. se esiste, prova a connetterti. Se la connessione **riesce**, c'è un'altra istanza viva:
   ritorna errore e non partire;
3. se la connessione **fallisce** (`ECONNREFUSED`), il socket è orfano: `remove_file` e binda.

Subito dopo il bind, `std::fs::set_permissions(&path, Permissions::from_mode(0o600))`.

Alla chiusura, `remove_file` del socket. Fallo in un punto che viene eseguito anche in caso di
errore: il più semplice è una struct guard con `impl Drop` che tiene il path, creata subito
dopo il bind. Non affidarti solo al ramo di uscita felice.

## Gestione di una connessione

```
scrivi la riga di hello
loop {
  leggi una riga (BufReader::read_line)
  riga vuota  -> continua
  EOF         -> esci dal loop, chiudi
  parse fallito -> rispondi invalid_params con id 0, esci dal loop
  ok -> crea oneshot, manda (req, sender) su tx, aspetta la risposta, scrivila
}
```

Due dettagli che contano:

- **Timeout sull'attesa della risposta**: avvolgi il `recv` del oneshot in
  `tokio::time::timeout(Duration::from_secs(5), ...)`. Se il main loop è morto o bloccato, la
  connessione non deve restare appesa per sempre. Su timeout rispondi `internal`.
- **Se `tx.send` fallisce** il main loop non c'è più: rispondi `internal` e chiudi.

Ogni connessione va gestita in un task suo (`tokio::spawn`), altrimenti un client lento blocca
tutti gli altri.

## Errori di parsing

Se la riga non è JSON valido, o la busta non ha `type: "request"`, la risposta è:

```json
{"type":"response","id":0,"error":{"code":"invalid_params","message":"..."}}
```

Non propagare mai il messaggio di errore di serde grezzo se contiene il payload: potrebbe
contenere variabili d'ambiente. Tronca il messaggio a ~200 caratteri.

## Criteri di accettazione

- `cargo build` pulito.
- Un test di integrazione (o una prova manuale con `nc -U`) che apre il socket, riceve
  l'hello, manda una richiesta con metodo inesistente e riceve `unknown_method`. A questo
  punto del piano il main loop non risponde ancora nulla di utile: va bene, si verifica il
  percorso di errore e il framing.
- Ammazzando il processo con `SIGKILL` e riavviandolo, il socket orfano viene ripulito e il
  bind riesce.
- Con due istanze sullo stesso path, la seconda si rifiuta di partire con un errore chiaro.
