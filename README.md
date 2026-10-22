# cortex9-lsm-engine

[![CI](https://github.com/federiconbaez/cortex9-lsm-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/federiconbaez/cortex9-lsm-engine/actions/workflows/ci.yml)
![License](https://img.shields.io/badge/license-Apache--2.0-blue)

Motor de almacenamiento **LSM-tree** en Rust: WAL + memtable, SSTables con **bloom filters**, **compactación por niveles** y manifest, probado **crasheando en cada punto de escritura** (incluidas entradas de directorio sin `fsync` y sectores rotos) y comparado con un modelo `BTreeMap`. Sin dependencias de runtime.

**LSM-tree** storage engine in Rust: WAL + memtable, SSTables with **bloom filters**, **leveled compaction** and a manifest, tested by **crashing at every write point** (including unsynced directory entries and torn sectors) and compared against a `BTreeMap` model. No runtime dependencies.

Parte de la serie Cortex-9 / Part of the Cortex-9 series: [github.com/federiconbaez](https://github.com/federiconbaez#cortex-9).

> **ES** primero · **EN** below

---

## ES

### Diseño

- **Escritura**: registro con CRC-32 en el WAL → `fsync(WAL)` → memtable (`BTreeMap`). Un `Ok` es durable (con `sync_writes`, que viene activado).
- **Flush**: la memtable se escribe como SSTable de nivel 0 → `fsync(tabla)` → se crea el WAL siguiente → `fsync(directorio)` → se agrega `{tabla en L0, log_number = WAL nuevo}` al manifest → `fsync(manifest)`. Recién después se borra el WAL viejo.
- **SSTable**: bloques de ~4 KiB, índice, bloom filter (10 bits/clave, k = 7) y footer; cada parte tiene su propio CRC-32, así que una tabla rota o truncada se detecta al abrirla o leerla.
- **Compactación por niveles**: L0 (tablas que se solapan) pasa a L1 cuando hay 4 tablas. Cada nivel Lᵢ ≥ 1 está ordenado y sin solapamientos, con un límite de 8 MiB × 10ⁱ⁻¹; al pasarse, una tabla (round-robin) se mezcla con las que se solapan en Lᵢ₊₁. Los tombstones se descartan solo si ningún nivel más profundo puede tener la clave. Las salidas se escriben y sincronizan, se hace `fsync` del directorio, se confirma `{borrar entradas, agregar salidas}` en el manifest y recién ahí se borran las entradas.
- **Manifest**: log de ediciones con CRC; al superar 64 KiB se rota a un `MANIFEST-N` nuevo con un snapshot completo. Al abrir se elige el de número más alto cuyo primer registro es un snapshot válido.
- **Recuperación**: carga el manifest, reproduce los WAL con número ≥ `log_number` hasta el primer registro dañado, hace flush de eso, arranca un WAL y un manifest nuevos y borra todo archivo que el manifest nuevo no referencia.
- `Vfs` es un trait: `FsVfs` (directorio real) y `SimVfs`. `SimVfs` modela tres cosas: los datos no sincronizados son volátiles; **crear o borrar un nombre es volátil hasta `fsync` del directorio**, como en Linux; y al crashear cada operación pendiente se **pierde, se conserva o se rompe** por sectores de 512 B, de forma independiente.

### Cómo se prueba

| Test | Qué hace |
|---|---|
| `crash::recovers_at_every_crash_point` | Por workload (150 puts de 1–200 B y deletes sobre 60 claves, con límites chicos para forzar flushes, compactaciones en 3 niveles y rotaciones del manifest): cuenta las N operaciones del file system y **crashea después de la operación k para cada k en 0..=N**. Cada punto recibe 4 recuperaciones (`LoseAll`, `KeepAll`, dos `Torn`). El estado recuperado tiene que ser **exactamente** lo confirmado, o lo confirmado + la escritura en vuelo; `check()` verifica niveles, metadatos y cada byte de cada tabla; después siguen 8 escrituras y otro crash. |
| 5 controles negativos | `skip_wal_sync`, `skip_table_sync`, `skip_dir_sync`, `early_delete` (borrar entradas antes del commit del manifest), `drop_tombstones_early`. Cada uno **tiene que** ser detectado por el mismo harness. |
| `model::matches_btreemap` (+ sin bloom) | proptest: hasta 400 pasos de put/delete/get/range/flush/reapertura/**corte de energía** contra `BTreeMap`. |
| `model::tombstone_bug_is_caught_without_crashes` | El bug de tombstones también lo detecta el modelo, sin crashes. |
| `model::bloom_filters_skip_most_absent_probes` | 22 tablas, 5 000 búsquedas de claves ausentes: menos del 2 % lee un bloque. |
| `sigkill::recovers_after_sigkill` | Proceso real escribiendo en disco, matado con `SIGKILL`: está todo lo confirmado y las claves forman un prefijo. |
| `fs`, unitarios | Reapertura con archivos reales y replay; formato de SSTable (cualquier byte alterado se detecta); framing y CRC; bloom; ediciones del manifest; modelo del `SimVfs`. |

27 tests. CI en ubuntu + macOS (fmt, clippy `-D warnings`, tests en debug y release) y build con MSRV 1.75.

### Números medidos

Máquina: VM Linux 6.12, Intel Xeon, 8 vCPU, ext4. Una corrida, un hilo.

- `cargo test --release`: **13 884 puntos de crash** en 30 workloads y **55 536 recuperaciones verificadas**, sin fallas. En esas corridas sin interrupción hubo 289 flushes, 191 compactaciones y 68 rotaciones del manifest. Los 5 controles negativos se detectan.
- Bloom filter: 4 957 de 4 995 sondeos de claves ausentes se saltearon (0.76 % leyó un bloque).
- `lsm-bench` (200 000 puts de 100 B sobre claves aleatorias, **sin fsync**, page cache caliente, sin cache de bloques ni compresión):
  - ~198 000 puts/s; amplificación de escritura 4.72 (WAL + flush + compactación / bytes de usuario).
  - ~103 000 gets aleatorios/s; ~790 000 gets de claves ausentes/s (473 466 sondeos salteados por bloom, 3 877 leyeron un bloque).
  - Scan completo de 126 273 claves: 0.11 s.
  - Con `fsync` por put: ~10 900 puts/s. El `fsync` de esta VM es barato; **no** es representativo de un disco físico.

### Límites (honestos)

- Escritor único; sin snapshots de lectura, iteradores perezosos ni concurrencia. `range` y la compactación cargan en memoria las entradas que tocan (correcto, pero no streaming).
- Sin cache de bloques ni compresión; cada lectura de bloque es un `pread`.
- Sin group commit: con `sync_writes` hay un `fsync` por escritura.
- Flush y compactación son sincrónicos, dentro de la escritura que los dispara.
- `SimVfs` modela el page cache y el directorio del SO; no modela un disco que miente sobre `fsync`. El test de `SIGKILL` cubre crash de proceso, no corte de energía real.
- `FsVfs` usa `std::os::unix`; no hay soporte Windows.

### Uso

```rust
use cortex9_lsm_engine::{Db, FsVfs, Options};

let mut db = Db::open(FsVfs::open("data".as_ref())?, Options::default())?;
db.put(b"k", b"v")?;
assert_eq!(db.get(b"k")?, Some(b"v".to_vec()));
db.delete(b"k")?;
```

```sh
cargo test --release -- --nocapture
cargo run --release --bin lsm-bench -- /tmp/lsm-bench 200000
```

### Licencia

Apache-2.0.

---

## EN

### Design

- **Write**: CRC-32 framed record to the WAL → `fsync(WAL)` → memtable (`BTreeMap`). `Ok` means durable (with `sync_writes`, on by default).
- **Flush**: memtable written as a level-0 SSTable → `fsync(table)` → next WAL created → `fsync(directory)` → `{table in L0, log_number = new WAL}` appended to the manifest → `fsync(manifest)`. Only then is the old WAL removed.
- **SSTable**: ~4 KiB blocks, index, bloom filter (10 bits/key, k = 7) and footer, each with its own CRC-32, so a torn or truncated table is detected when opened or read.
- **Leveled compaction**: L0 (overlapping tables) goes into L1 at 4 tables. Every level Lᵢ ≥ 1 is sorted and disjoint, limited to 8 MiB × 10ⁱ⁻¹; when over, one table (round-robin) is merged with its overlaps in Lᵢ₊₁. Tombstones are dropped only when no deeper level can hold the key. Outputs are written and synced, the directory is synced, `{delete inputs, add outputs}` is committed to the manifest, and only then are the inputs removed.
- **Manifest**: CRC'd edit log; past 64 KiB it rotates to a new `MANIFEST-N` holding a full snapshot. Open picks the highest-numbered one whose first record is a valid snapshot.
- **Recovery**: load the manifest, replay WALs numbered ≥ `log_number` up to the first damaged record, flush that, start a fresh WAL and manifest, and delete every file the new manifest does not reference.
- `Vfs` is a trait: `FsVfs` (a real directory) and `SimVfs`. `SimVfs` models three things: unsynced data is volatile; **creating or removing a name is volatile until the directory is `fsync`ed**, as on Linux; and on a crash each pending operation is independently **lost, kept, or torn** at 512 B sector granularity.

### How it is tested

| Test | What it does |
|---|---|
| `crash::recovers_at_every_crash_point` | Per workload (150 puts of 1–200 B and deletes over 60 keys, with tiny limits to force flushes, compactions into 3 levels and manifest rotations): count the N file-system operations and **crash after operation k for every k in 0..=N**. Each point gets 4 recoveries (`LoseAll`, `KeepAll`, two `Torn`). The recovered state must be **exactly** the acknowledged state, or acknowledged + the in-flight write; `check()` verifies levels, metadata and every byte of every table; then 8 more writes and another crash. |
| 5 negative controls | `skip_wal_sync`, `skip_table_sync`, `skip_dir_sync`, `early_delete` (remove inputs before the manifest commit), `drop_tombstones_early`. Each **must** be caught by the same harness. |
| `model::matches_btreemap` (+ without bloom) | proptest: up to 400 steps of put/delete/get/range/flush/reopen/**power cut** vs `BTreeMap`. |
| `model::tombstone_bug_is_caught_without_crashes` | The tombstone bug is also caught by the model, with no crashes. |
| `model::bloom_filters_skip_most_absent_probes` | 22 tables, 5,000 lookups of absent keys: under 2 % read a block. |
| `sigkill::recovers_after_sigkill` | A real process writing to disk is killed with `SIGKILL`; every acknowledged key is present and keys form a prefix. |
| `fs`, unit tests | Real-file reopen and replay; SSTable format (any flipped byte is detected); framing and CRC; bloom; manifest edits; `SimVfs` model. |

27 tests. CI on ubuntu + macOS (fmt, clippy `-D warnings`, debug and release tests) and an MSRV 1.75 build.

### Measured numbers

Machine: Linux 6.12 VM, Intel Xeon, 8 vCPU, ext4. One run, one thread.

- `cargo test --release`: **13,884 crash points** across 30 workloads and **55,536 recoveries checked**, no failures. The uninterrupted runs did 289 flushes, 191 compactions and 68 manifest rotations. All 5 negative controls are caught.
- Bloom filter: 4,957 of 4,995 absent-key probes skipped (0.76 % read a block).
- `lsm-bench` (200,000 puts of 100 B on random keys, **no fsync**, warm page cache, no block cache or compression):
  - ~198,000 puts/s; write amplification 4.72 (WAL + flush + compaction / user bytes).
  - ~103,000 random gets/s; ~790,000 absent-key gets/s (473,466 probes skipped by bloom, 3,877 read a block).
  - Full scan of 126,273 keys: 0.11 s.
  - With `fsync` per put: ~10,900 puts/s. This VM's `fsync` is cheap; **not** representative of a physical disk.

### Limits (honest)

- Single writer; no read snapshots, lazy iterators or concurrency. `range` and compaction load the entries they touch into memory (correct, not streaming).
- No block cache or compression; every block read is a `pread`.
- No group commit: with `sync_writes`, one `fsync` per write.
- Flush and compaction run synchronously inside the write that triggers them.
- `SimVfs` models the OS page cache and directory; it does not model a disk that lies about `fsync`. The `SIGKILL` test covers a process crash, not real power loss.
- `FsVfs` uses `std::os::unix`; no Windows support.

### Usage

See the Spanish section above for the snippet and commands.

### License

Apache-2.0.
