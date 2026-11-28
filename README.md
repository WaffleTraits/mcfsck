# mcfsck

A fast, multi-threaded checker for Minecraft Java Edition worlds. It reads every chunk the way the game
does, reports the ones the game cannot load, repairs those that can be repaired without losing anything,
and can remove the rest safely so the game generates them anew.

```
mcfsck tui  /srv/minecraft/world     # full-screen: scan, browse, preview, fix
mcfsck scan /srv/minecraft/world     # list every problem (read-only)
mcfsck fix  /srv/minecraft/world --repair           # dry run: what can be repaired without loss
mcfsck fix  /srv/minecraft/world --repair --apply
mcfsck fix  /srv/minecraft/world                    # dry run: what would have to be removed
mcfsck fix  /srv/minecraft/world --apply
```

* **Fast.** One worker per core, one reused decompressor per worker, no NBT tree is ever built.
  Around 0.5 GiB/s of region files per core-second is typical; 11 GiB (1.6 million chunks) takes about
  20 seconds on a 32-thread desktop.
* **Safe.** Scanning only reads. Fixing is a dry run unless you say otherwise, refuses to run while the
  world is open in a game or server, backs every file up first, and changes nothing but the 4-byte
  header entries of the chunks it removes.
* **Accurate.** Every check mirrors what the game itself does when it loads a chunk. It was run over
  157 real worlds from 1.7.10 to 26.3, vanilla and heavily modded (1.6 million chunks, 76 million block
  and biome containers) without a single false alarm.

Works with every Anvil world (1.2 and newer), the classic folder layout and the 26.1+
`dimensions/<namespace>/<name>/` layout, all chunk compression schemes (gzip, zlib, none, LZ4), and
oversized chunks in `.mcc` files. `WORLD` can be a world folder, a dimension folder, a region folder or a
single `r.X.Z.mca` file.

> Bukkit-style servers keep the Nether and the End in their own folders (`world_nether`, `world_the_end`).
> Those are separate worlds: scan each one.

## Installing

mcfsck needs [Rust](https://rustup.rs) 1.89 or newer. With Cargo, one command builds it and puts
`mcfsck` in `~/.cargo/bin` (which rustup adds to your `PATH`):

```sh
cargo install --locked --git https://github.com/WaffleTraits/mcfsck
```

Run the same command again to update; `cargo uninstall mcfsck` removes it.

To build from a checkout instead:

```sh
git clone https://github.com/WaffleTraits/mcfsck
cd mcfsck
cargo install --locked --path .    # or: cargo build --release  ->  target/release/mcfsck
cargo test
```

It builds on Linux, macOS and Windows and has no dependencies outside of Cargo's.

## What it finds

A full scan checks, for every chunk in every `region`, `entities` and `poi` file:

| Code | Severity | What is wrong |
| --- | --- | --- |
| `header_overlap` | error | The header entry points into the file header. |
| `zero_sectors` | error | The header entry reserves no sectors. |
| `out_of_bounds` | error | The chunk should start beyond the end of the file. |
| `cut_off` | error | The file ends inside the chunk's 5-byte header. |
| `missing_stream` | error | Sectors are reserved but empty (typically left by a crash). |
| `negative_length` | error | The chunk declares a negative length. |
| `truncated` | error | The chunk declares more data than fits in its sectors or in the file. |
| `bad_compression` | error | Unknown compression scheme. |
| `external_missing` | error | The chunk is stored in a `c.X.Z.mcc` file that does not exist. |
| `corrupt` | error | The compressed data is damaged (bad stream, failed checksum, or ends too early). |
| `bad_nbt` | error | The decompressed data is not valid NBT (truncated, unknown tags, negative lengths, nested deeper than 512). |
| `no_position` | error | Terrain without `xPos`/`zPos`, or entity data without `Position`. |
| `wrong_position` | error | The data belongs to a different chunk than the slot it is stored in. |
| `bad_section` | error | A section's block or biome data does not have the size its palette requires (1.18+). |
| `no_header` | error | A region file shorter than its 8 KiB header. |
| `unreadable` | error | The operating system refused to read the file. No fix. |
| `truncated_stream` | warning | The end marker of the compressed stream is missing, but the chunk data is complete; the game still loads it. |
| `shared_sectors` | warning | Two header entries claim the same sectors; this chunk reads fine, the other one is usually the broken one. |
| `orphan_mcc` | warning | A `.mcc` file nothing refers to. |
| `unverified` | warning | Custom compression or larger than 512 MiB: could not be looked into. No fix. |

Empty (0-byte) region files are normal - the game creates them - and are only counted.

`--quick` checks the region and chunk headers only (the first nine rows, plus `no_header`,
`shared_sectors`, `orphan_mcc`). It does not decompress anything, so it is several times faster and is a
good first look at a huge world on a slow disk; it cannot see damage inside the chunk data.

## scan

```sh
mcfsck scan world                    # text; exit code 0 = clean, 1 = problems, 2 = could not scan
mcfsck scan world --errors-only
mcfsck scan world -o report.jsonl    # also write JSON lines
mcfsck scan world --json | jq .      # JSON lines on stdout
mcfsck scan world -j 4               # four worker threads (default: all cores)
```

The JSON-lines report starts with a header object (`world`, `chunks`, `errors`, `warnings`, ...) followed
by one object per finding: `n`, `severity`, `kind`, `problem`, `detail`, `file`, `dimension`, `store`,
`region`, `chunk`, `fix`.

Scanning a world that is being played is allowed (nothing is written), but a chunk the game is saving at
that moment can look damaged. mcfsck says so when it notices the world is open.

## fix --repair

Some errors are damage to the bookkeeping around a chunk while the chunk itself is intact. `--repair`
puts those right and removes nothing:

```sh
mcfsck fix world --repair              # dry run: what can be repaired, what cannot and why
mcfsck fix world --repair --apply      # asks, then does it (--yes for scripts)
mcfsck fix world --repair --kind wrong_position --apply
```

| Error | Repair |
| --- | --- |
| `negative_length`, `truncated`, `missing_stream` | A zlib or gzip stream marks its own end. If the stream behind the bad length field is complete, the length is corrected. |
| `bad_compression` | If the data is an intact chunk in one of the known schemes, the scheme marker is corrected. |
| `truncated`, `zero_sectors` | If the chunk's data runs on into sectors nothing else uses, the sector count in the header is corrected. |
| `header_overlap`, `zero_sectors`, `out_of_bounds` | The header entry leads nowhere. The sectors no entry claims are searched for a complete chunk with this position, and the entry is pointed at it. With several copies the one with the highest `LastUpdate` wins; if they do not say, nothing is done. |
| `wrong_position` | The data is filed under the chunk it says it is, if that chunk is in the same region file and has no entry or unloadable data. If that chunk already points at the very same data, the extra entry is dropped. Either way the emptied slot is then searched for its own data as above. |

Everything else - damaged compressed data, damaged NBT, a missing `.mcc` file - cannot be repaired: the
format keeps no redundancy, so the bytes are gone. Those are listed with the reason and left untouched;
`mcfsck fix` (below) removes them when you decide to.

A repair is only made if the chunk afterwards passes every check of the scan. It writes nothing but the
5-byte header of a chunk and 4-byte entries of the region header: chunk data is never moved or rewritten,
and the file keeps its size. Dry run, backups, the world lock and the "changed since the scan" rule work
exactly as described under [Safety](#safety).

One caveat: a chunk found again in unused sectors is the newest complete copy still in the file. If the
game saved the chunk once more after writing that copy and it is that last save which was lost, you get
the state before it - older, but real, instead of freshly generated terrain.

## fix

A chunk the game cannot load is gone already (unless `--repair` can bring it back); the only question is whether the game regenerates it
quietly, spams the log on every load, or crashes. `fix` takes such chunks out of their region file, the
same way the game deletes a chunk, so that fresh terrain is generated there the next time someone visits.

```sh
mcfsck fix world                       # dry run (the default): prints exactly what would happen
mcfsck fix world --apply               # asks, then does it
mcfsck fix world --apply --yes         # for scripts
mcfsck fix world --kind corrupt,bad_nbt --apply
mcfsck fix world --warnings --apply    # also unused .mcc files and odd-but-loadable chunks
```

By default only **errors** are fixed. What a fix does:

* **Chunks** - the chunk's entry in the region header is zeroed. For a terrain chunk the entity and
  point-of-interest data of the same chunk goes too (`--keep-linked` to leave it): mobs, item frames,
  bed and portal records would otherwise outlive the terrain they belong to. Its `.mcc` file, if any, is
  removed with it.
* **Files** - a region file without a header, or (with `--warnings`) an unused `.mcc` file, is deleted.
* **`unreadable` / `unverified`** - nothing; there is nothing this tool can safely remove.

### Safety

* **Stop the server first.** mcfsck takes the world's `session.lock` the way the game does and refuses to
  write if a game or server holds it (`--force` overrides; do not use it on a running server).
* **Dry run first.** Without `--apply` nothing is opened for writing. The dry run performs every check
  the real run performs, so its output is what will happen.
* **Backups.** Before a file is changed or deleted it is copied to
  `WORLD/mcfsck-backups/<UTC date-time>/` under its relative path. If that copy fails, the file is left
  alone. To undo, copy the files back. `--no-backup` skips this.
* **Nothing stale.** A chunk is only removed if its header entry (location and timestamp) is still
  exactly what the scan saw; a file only if it is still what the scan described. Anything else is
  skipped with "changed since the scan".
* **Minimal writes.** Removing a chunk overwrites 4 bytes of location and 4 bytes of timestamp in the
  region header and nothing else. No region file is rewritten, moved or truncated, so chunks that were
  not selected cannot be affected, and an interruption at any point leaves a consistent file.

## tui

```sh
mcfsck tui world
mcfsck tui world --quick --theme mc
```

The whole flow in one screen, keyboard and mouse:

| Key | |
| --- | --- |
| Up/Down, PgUp/PgDn, Home/End, wheel | move |
| Space, right-click, click `[ ]` | select |
| `a` / `n` / `i` / `e` | select all shown / none / invert / the errors shown |
| Enter, double-click | everything about the finding, with a `/tp` command to go and look |
| `/` | filter: words that must all appear (`error`, `nether`, `entities`, `corrupt`, `12,-5`, a file name) |
| `s` / `S`, click a column header | sort / reverse |
| `p` | repair the selection (or the current finding) where that loses nothing: **shows a dry run first**, Enter applies it |
| `d` | remove the selection (or the current finding): **shows a dry run first**, Enter applies it |
| `r` | rescan |
| `c` | what the scan looked into |
| `w` | save the JSON-lines report (`-o FILE`, default `mcfsck-report.jsonl`) |
| `m` / `t` | mouse on/off / theme (default or Midnight Commander) |
| `?` | help |
| `q`, Ctrl-C | quit |

The same safety rules as `fix` apply, and the options `--no-backup`, `--force` and `--keep-linked` work
here too. While changes are being written the interface cannot be interrupted.

## How the checks relate to the game

mcfsck follows the game's own loading code (`RegionFile`, `NbtIo`, `PalettedContainer`):

* Header entries are rejected under the same conditions under which the game drops them on opening a
  region file, and chunk records under the same conditions under which `RegionFile` returns no data.
* zlib streams are verified including their Adler-32 checksum, gzip including its CRC, and LZ4 blocks
  including lz4-java's XXH32 block checksums (tested against a stream written by lz4-java itself).
* NBT is walked completely with the limits the game enforces.
* A paletted container of `n` palette entries must have exactly `ceil(size / floor(64 / bits))` longs,
  where `bits` is `ceil(log2 n)` (at least 4 for blocks) - the size `PalettedContainer` insists on. This
  check only runs for chunks saved by 1.18 (data version 2860) or newer.

What mcfsck does not judge: whether block states, entities or items inside a well-formed chunk make
sense to the game or to a mod. For illegal items, see `mcscan`.
