# 02. Stato dei processi: exit code, started_at, log su file

Task 1. Tocca `src/proc.rs`, `src/config.rs` e due righe in `vendor/pty/src/lib.rs`.
Non dipende dal protocollo: si può fare e rilasciare da solo.

Alla fine di questo task ogni processo ha un file di log leggibile dall'esterno e, quando
muore, si sa perché.

## 1a. Esporre il nome del segnale nel pty vendored

`vendor/pty/src/lib.rs:161` definisce:

```rust
pub struct ExitStatus {
  code: u32,
  signal: Option<String>,
}
```

C'è già `exit_code()` (riga 189) e `success()` (riga 181), ma `signal` è privato e non ha
accessore. Aggiungine uno dentro l'`impl ExitStatus` esistente:

```rust
/// Returns the signal name that terminated the process, if any.
pub fn signal(&self) -> Option<&str> {
  self.signal.as_deref()
}
```

Il crate è vendored nel workspace (`vendor/pty`), quindi è una modifica locale senza attriti.
Nota che `signal` è il **nome** del segnale (`"Terminated"`, da `strsignal`), non il numero.
Nel JSON esce come stringa.

## 1b. Propagare l'exit status

Oggi in `src/proc.rs:99-108`:

```rust
spawn(move || {
  // Block until program exits
  let _status = child.wait();
  running.store(false, Ordering::Relaxed);
  let _result = tx.send((id, ProcUpdate::Stopped));
});
```

Lo status viene scartato. Cambia `ProcUpdate` (`src/proc.rs:167-172`) in:

```rust
#[derive(Debug)]
pub enum ProcUpdate {
  Render,
  Stopped { exit_code: Option<i32>, signal: Option<String> },
  Started,
}
```

e nel thread di attesa manda i valori veri. `child.wait()` restituisce
`Result<ExitStatus, Error>`: su `Err` manda entrambi i campi a `None`.

Attenzione: `ExitStatus::exit_code()` è `u32`, il campo JSON è `i32`. Converti con `as i32`,
i codici di uscita reali stanno in `0..=255`.

Poi aggiungi a `Proc` (`src/proc.rs:142-156`):

```rust
pub last_exit_code: Option<i32>,
pub last_signal: Option<String>,
```

inizializzati a `None` in `Proc::new` (`src/proc.rs:194`), azzerati in `Proc::start`
(`src/proc.rs:237`) prima dello spawn, e riempiti in `App::handle_proc_update`
(`src/app.rs:791`) quando arriva `ProcUpdate::Stopped`.

Il match su `ProcUpdate::Stopped` in `src/app.rs:791` va aggiornato alla nuova forma. È
l'unico punto che lo consuma, il compilatore ti guida.

## 1c. Momento di avvio

Aggiungi a `Inst` (`src/proc.rs:22-30`):

```rust
pub started_at: std::time::SystemTime,
```

valorizzato con `SystemTime::now()` in `Inst::spawn`, subito dopo `spawn_command`
(`src/proc.rs:62`). Nel JSON esce come epoch secondi:
`started_at.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())`.

Niente `chrono`. `std::time` basta e non aggiunge dipendenze.

## 1d. Tee dell'output del pty su file

È il pezzo che dà più valore. Il reader loop sta in `src/proc.rs:68-97`:

```rust
spawn_blocking(move || {
  let mut buf = [0; 4 * 1024];
  loop {
    if !running.load(Ordering::Relaxed) { break; }
    match reader.read(&mut buf[..]) {
      Ok(count) => {
        if count > 0 {
          if let Ok(mut vt) = vt.write() {
            vt.process(&buf[..count]);
            ...
```

Prima di `spawn_blocking`, apri il file (se configurato) e spostalo dentro la closure.
Dentro il ramo `count > 0`, scrivi `&buf[..count]` anche sul file.

Regole:

- **Bytes grezzi, ANSI incluso.** Non filtrare le sequenze di escape qui: si perde
  informazione e serve un parser. Lo strip lo fa il consumatore (`monade stack logs`).
- **Apertura in append**, `OpenOptions::new().create(true).append(true)`.
- **Marker di avvio**: subito dopo l'apertura, scrivi una riga
  `\n=== mprocs: <name> started, pid=<pid>, at=<epoch> ===\n`. Serve a separare le esecuzioni
  successive dentro lo stesso file.
- **Tetto alla dimensione**: prima di aprire in append, se il file esiste ed è più grande di
  `log_max_bytes` (default 8 MiB), troncalo. Un semplice `metadata().len() > max` seguito da
  `.truncate(true)`. Senza questo i file crescono all'infinito su una macchina di sviluppo,
  che è esattamente il tipo di residuo che si vuole evitare.
- **Errori di scrittura**: logga una volta con `log::warn!` e poi continua a leggere ignorando
  il file. Un disco pieno non deve far cadere il processo né la TUI.
- Niente `flush()` esplicito a ogni write oltre a quello implicito di `File`: la latenza non
  conta, i log si leggono a posteriori.

## 1e. Configurazione

In `Config` (`src/config.rs:19-25`) aggiungi:

```rust
pub log_dir: Option<std::path::PathBuf>,
pub log_max_bytes: u64,   // default 8 * 1024 * 1024
```

Va popolato da `RunOptions` (task 4), **non** dal file YAML: è monade-cli che decide dove
mettere i log, non lo stack. Metti comunque i default in `Config::make_default`
(`src/config.rs:69`) e in `Config::from_value` (`src/config.rs:58`).

Il path per processo lo calcola il fork: `log_dir.join(format!("{}.log", sanitize(name)))`,
dove `sanitize` sostituisce con `_` tutto ciò che non è `[A-Za-z0-9._-]`. I nomi dei processi
arrivano da YAML scritto da umani e possono contenere spazi o slash.

`Proc` deve tenere il path calcolato (`pub log_file: Option<PathBuf>`) perché `ls` lo espone
nella risposta e perché serve a ogni riavvio, non solo al primo spawn.

## Criteri di accettazione

- `cargo build` pulito, nessun warning nuovo.
- Avviando con una `log_dir`, i file compaiono e contengono l'output dei processi.
- Riavviando un processo dalla TUI, nel file compare un nuovo marker `=== mprocs: ... ===`.
- Un file di log oltre il tetto viene troncato al riavvio del processo, non cresce all'infinito.
- Fermando un processo con `x`, `last_exit_code` è valorizzato (verificabile con un
  `log::info!` temporaneo, o rimandato al task 3 quando `ls` lo espone).
- Senza `log_dir` configurata, il comportamento è identico a oggi e non si apre alcun file.
