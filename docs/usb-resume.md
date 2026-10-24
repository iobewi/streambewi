# USB MSC: resuming at a remembered position (#31)

The Metronic remembers the file offset it had reached (a byte offset, applied as is across stations and bit
rates). When it reopens the disk it may ask for that offset as its *first audio read*, before any sequential
read. A live stream cannot serve it: a position in the past has left the window, a position in the future does
not exist yet. Waiting makes the host reset the bus after about 3 s and ask again (a re-enumeration loop);
answering zeroes makes it give up.

`streambewi_core::Resume` (always on in the product, `RESUME_REBASE` in `app/src/stream.rs`) therefore fixes the
origin of an MSC session once, on its first audio read, when the stream could not serve that read.

| Event | Rule |
|---|---|
| New session | `begin_session` of the file: MSC connected, or the host reopening sector 0 |
| Metadata | Reads below the volume data area (boot, FAT, root) never reach the file; sectors 0 to 7 of the file (`HEAD_SECTORS`) are never a resume |
| Trigger | The first read of a file sector `>= HEAD_SECTORS` of the session whose end lies beyond `read_end + MAX_LEAD_BYTES` |
| Why that limit | The producer may run `MAX_LEAD_BYTES` (80 KiB) past what the session has read. A sector beyond that stays `Pending` for as long as the host does not read: for ever |
| Operation | `begin_session()` (origin at the live edge minus 64 KiB of history) and the file offset the host asked for is bound to it (`shift`). Audio is real stream bytes, never fabricated |
| Reachable first read | No rebase, however close to the live edge: the producer gets there within a moment (at most 80 KiB, a few seconds at 16 KB/s) |
| Refused | In the last 1 MiB of the file (Windows tail probe: zeroes at once, as before); or the origin is already fixed by an earlier audio read |
| Before the origin | `Expired`, zeroes. Sector 0 reopens the file: the origin returns to sector 0 |
| Later reads | Same mapping (`offset - shift`) for re-reads and multi-sector commands, until the window moves past them (`Expired`) |
| Seeks later in a session | The unchanged policy: wait if near, zeroes beyond `FAR_AHEAD_BYTES`. The origin never moves again |

Invariants of a session: at most one rebase; the offset-to-stream mapping never changes after the first audio
read; a sector already served answers the same bytes; no byte of a sector before the origin is presented as audio.

Not addressed here, tracked in #31: the host stopping after its first 16 audio sectors (also seen without a
rebase, with valid MP3 data and correct answers), and the rare absence of enumeration.
