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

Invariants of a logical file session: the first audio read can commit its origin (including the remembered-offset
rebase). Within the committed mapping, a retained sector answers the same bytes; expired sectors are not
presented as audio. A recognized reopening starts a new logical file session, even if the USB connection
itself stays up.

Terminology:

| Concept | Meaning |
|---|---|
| Production position (written/live) | Absolute count of bytes accepted into the rolling window, not bytes received from HTTP |
| Consumption position (read_end/consumed) | Highest successfully read absolute byte end in the current window session |
| File origin and shift | Mapping from logical file offsets into the rolling window |
| MSC connection | USB attachment and protocol lifecycle; not necessarily one logical file session |
| Reopening / REBASE | A new logical mapping under the explicit policies described here |
| Drain | Read and discard excess network bytes without advancing the production position |
| Starvation recovery | Detach after a pending read under a silent producer, then wait for a fresh prebuffer before reattaching |

StreamBeWI is a live radio adapter, not a time-shift cache. Temporal audio continuity is not guaranteed;
stable retained bytes within a committed mapping and recovery of the audible service are the goals.

## Inactive consumer and head-only retry (#47)

The network pump waits for room for at most two seconds of continuous saturation
(`CONSUMER_IDLE_LIMIT`). It then reads and
discards HTTP body bytes while the window remains full. The retained bytes and virtual offsets do not
move. On forward consumer progress, normal writes resume (at most one in-flight drain chunk can be
discarded). This is a live radio, not a time-shift recorder: audio received during prolonged inactivity
is skipped, and the next append may introduce an MP3 discontinuity. Hardware playback qualification
is required. Genuine EOF, body errors and read timeout remain visible during drain.

The product disk wrapper recognizes the observed Metronic root/FAT/root/file-head sequence. A new
such sequence after at least two seconds without a file read (`HEAD_REOPEN_IDLE_LIMIT`, independent of
the saturation timeout despite the same initial value) can restart a **head-only** session
(one to `HEAD_SECTORS` sectors consumed, at most 4 KiB, no shifted origin), only if the
retained origin can move forward by at least a sector. Q47-06 showed that a refused open may read
three sectors before stopping; the previous single-sector guard prevented every subsequent retry. Immediate or isolated re-reads keep the same bytes, and a session that progressed
beyond the recognition zone cannot use this retry rule. Existing expired-sector-zero reopening is unchanged.
This is a bounded-memory recovery opportunity, not a guarantee that a reader accepts the new content.
No USB detach is requested by either policy. The existing starvation recovery is only eligible when
the held session has enough free space for the complete recovery prebuffer; a far-ahead pending
read against a full window must not initiate an unrecoverable disconnect.

Not addressed here, tracked in #31: the host stopping after its first 16 audio sectors (also seen without a
rebase, with valid MP3 data and correct answers), and the rare absence of enumeration.
