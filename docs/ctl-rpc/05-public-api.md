# 05. API pubblica e opzioni

Task 4. Tocca `src/lib.rs` e `src/config.rs`. È il contratto verso `monade-cli`.

## Il vincolo

`monade-cli` oggi fa esattamente questo, in due punti
(`../monade-cli/src/start/mod.rs:82` e `:144`):

```rust
use mprocs::run_mprocs;
run_mprocs(final_mprocs_path.to_str().unwrap()).await.expect("Error starting stack (mprocs)");
```

`run_mprocs(&str)` (`src/lib.rs:44`) **deve restare com'è**, stessa firma e stesso
comportamento. Un utente che aggiorna il crate senza cambiare codice non deve accorgersi di
niente. Le nuove capacità arrivano da una funzione affiancata.

## `RunOptions` e `run_mprocs_with`

In `src/lib.rs`:

```rust
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
  /// Path del mprocs.yaml da caricare.
  pub yaml_path: PathBuf,
  /// Se presente, apre il socket di controllo su questo path.
  pub ctl_socket: Option<PathBuf>,
  /// Se presente, ogni processo scrive il suo output in <log_dir>/<nome>.log
  pub log_dir: Option<PathBuf>,
  /// Tetto oltre il quale un file di log viene troncato al riavvio del processo.
  /// `None` usa il default (8 MiB).
  pub log_max_bytes: Option<u64>,
}

pub async fn run_mprocs_with(opts: RunOptions) -> anyhow::Result<()>
```

`RunOptions` deve essere costruibile campo per campo con `..Default::default()`, così
aggiungere opzioni in futuro non rompe i chiamanti. Per lo stesso motivo, marcala
`#[non_exhaustive]` solo se sei disposto a fornire un costruttore: altrimenti lascia perdere,
è un crate consumato da un solo progetto.

Riscrivi `run_mprocs` come wrapper sottile:

```rust
pub async fn run_mprocs(yaml_path: &str) -> anyhow::Result<()> {
  run_mprocs_with(RunOptions { yaml_path: yaml_path.into(), ..Default::default() }).await
}
```

Il corpo attuale di `run_mprocs` (`src/lib.rs:44-72`) diventa il corpo di `run_mprocs_with`,
con in più il travaso di `ctl_socket`, `log_dir` e `log_max_bytes` dentro `Config` prima di
chiamare `run_client_and_server`.

## Perché le opzioni non stanno nel YAML

`log_dir` e `ctl_socket` sono decisioni dell'**host** (monade-cli sceglie dove mettere i log e
i socket sotto `~/.config/monade/`), non dello stack. Il `mprocs.yaml` che monade genera è un
artefatto temporaneo rigenerato a ogni avvio: metterci dentro path assoluti della macchina lo
renderebbe non condivisibile.

`Config` li riceve dal codice, non dal parser YAML. In `Config::from_value`
(`src/config.rs:28`) e `Config::make_default` (`src/config.rs:69`) valorizzali con i default
(`None`, 8 MiB) e sovrascrivili in `run_mprocs_with` dopo la costruzione.

## Avvio dei due task

`run_client_and_server` (`src/lib.rs:150`) oggi lancia client e server. Deve diventare:

```
crea il canale ctl (mpsc unbounded)
crea exit_trigger / exit_listener   (oppure riusa quello che App::run già crea)
spawn client_main
spawn server_main(config, keymap, srv_tx, srv_rx, ctl_rx)
se opts.ctl_socket è Some -> spawn ctl_server_main(path, ctl_tx, exit_listener)
join
```

Il `triggered::Listener` per fermare il ctl server: `App::run` ne crea già uno a
`src/app.rs:54-56` per il ctl TCP. Puoi crearne un secondo a questo livello, oppure spostare
la creazione qui e passarlo giù. La seconda è più pulita ma tocca più codice: scegli tu, basta
che il ctl server **muoia insieme all'app** e che il file socket venga rimosso.

Se il ctl server fallisce a partire (socket occupato da un'istanza viva), l'intera chiamata
deve fallire con un errore leggibile. Non degradare in silenzio: un utente che si aspetta di
poter interrogare lo stack e non può, deve saperlo subito.

## Modifiche lato monade-cli (per contesto, non farle qui)

Quando questo crate sarà pubblicato, `monade-cli` cambierà `src/start/mod.rs` in:

```rust
run_mprocs_with(RunOptions {
  yaml_path: path.into(),
  ctl_socket: Some(runtime_dir.join(format!("{}.sock", stack_name))),
  log_dir: Some(log_dir),
  ..Default::default()
}).await
```

più un registro delle istanze in `~/.config/monade/run/<stack>.json` con pid e cwd, per
permettere a `monade stack ps` di trovare il socket da qualsiasi directory e di ripulire i
socket orfani. **Quel lavoro sta nel repo monade-cli, non qui.** Serve solo a farti capire
perché l'API ha questa forma.

## Criteri di accettazione

- `run_mprocs(path)` compila e si comporta come prima, senza socket e senza log su file.
- `run_mprocs_with` con `ctl_socket: None` e `log_dir: None` è indistinguibile da `run_mprocs`.
- Con entrambi valorizzati, socket e log compaiono dove indicato.
- Alla chiusura della TUI il file socket non esiste più.
- Provare a partire due volte sullo stesso socket path fallisce con un errore comprensibile.
