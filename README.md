# Chess puzzle solver

A command-line tool for testing chess engines by puzzles.

## Build

```sh
cargo build --release
```

The binary is named `cps` and lands in `target/release/`.
Requires Rust 1.85 or newer.

## Puzzle database

Puzzles come from the [Lichess open database](https://database.lichess.org/),
distributed as a Zstandard-compressed CSV in the Lichess puzzle format. Download
and unpack it into `data/`:

```sh
mkdir -p data
curl -L https://database.lichess.org/lichess_db_puzzle.csv.zst \
  | zstd -d -o data/lichess_db_puzzle.csv
```

The archive is roughly 300 MB and expands to about 1 GB, holding more than 6
million puzzles. Each row has the following columns:

```
PuzzleId,FEN,Moves,Rating,RatingDeviation,Popularity,NbPlays,Themes,GameUrl,OpeningTags
```

`FEN` is the position before the opponent moves, and the first move in `Moves`
is that opponent move. Apply it to the FEN to get the position the solver is
asked about — the solution starts from the second move.

## Usage

| Flag | Default | Meaning |
| --- | --- | --- |
| `--engine "COMMAND [ARGUMENTS]"` | required | UCI engine to run, with arguments if it needs any |
| `--database PATH` | `data/lichess_db_puzzle.csv` | puzzle database to read |
| `--depth PLIES` | `16` | search depth per puzzle |
| `--count NUMBER` | `100` | how many puzzles to test |
| `--skip NUMBER` | `0` | puzzles to pass over before the first tested one |
| `--workers NUMBER` | available cores | engine processes to run in parallel |
| `--hash MEGABYTES` | `16` | shorthand for `--option Hash=MEGABYTES` |
| `--option NAME=VALUE` | none | any other UCI option, repeat the flag for more |
| `--show-failed` | off | list unsolved puzzles after the run |
| `--help` | | print the usage line and exit |

```sh
cps --engine stockfish --depth 16 --count 500 --workers 6 --show-failed
```

Every worker runs its own engine process and takes the next untested puzzle as
soon as it finishes the previous one, so a slow puzzle holds up nobody else. The
default comes from `std::thread::available_parallelism`, which respects cgroup
limits and CPU affinity, so a run inside a container gets its own quota rather
than the whole host. Asking for more workers than puzzles starts as many workers
as there are puzzles.

Keep the engine itself single-threaded. Puzzles are searched to a fixed depth,
and a parallel search buys strength per unit of *time*, not a faster time to a
given depth: helper threads widen the tree, so `Threads=6` finishes depth 16
slower than `Threads=1` does. Spend the cores on `--workers` instead, where the
speedup is nearly linear, and keep `--workers` at or below the number of physical
cores.

`Hash` is set to 16 MB unless `--hash` says otherwise, so engines that ship with
different defaults still get compared on equal terms. It is a per-process budget:
six workers with `--hash 1024` ask the machine for 6 GB, so size it against memory
divided by workers, not against memory alone.

Unsolved puzzles are listed in the order of the database columns — id, FEN, the
recorded moves and rating — followed by the move the engine played and the game
link:

```
000VW | r4r2/1p3pkp/p5p1/3R1N1Q/3P4/8/P1q2P2/3R2K1 b - - 3 25 | g6f5 d5c5 c2e4 h5g5 g7h8 g5f6 | 2869 | played h5g5 | https://lichess.org/e9AY2m5j/black#50
```
