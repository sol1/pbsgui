pbsgui backs up Windows files and Microsoft SQL Server to a Proxmox Backup Server
(PBS), with browse and point-in-time restore.

## New in this release

- **Much faster restores.** Restoring from PBS now downloads chunks concurrently
  (the read-side of how backups already upload) and overlaps the network read with
  the disk write, instead of fetching one chunk at a time and waiting a full network
  round-trip for each. A restore that was bursty and stalled on network latency is
  now steady and limited by your bandwidth or disk. Both the "Save to file" export
  and the restore back into SQL Server benefit: the in-place restore now prefetches
  chunks ahead of SQL Server so it rarely waits on the network.

Installers are attached below. See the
[README](https://github.com/sol1/pbsgui#install) for which one to choose, plus
setup and upgrade steps.
