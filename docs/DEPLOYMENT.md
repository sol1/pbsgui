# Deployment

How to install and run pbsgui in production against one SQL Server host. For a
fleet, or to move backup CPU off the database server, read this first, then
[RELAY.md](RELAY.md).

## Requirements

- **Proxmox Backup Server 4.2 or newer.** Older 3.x servers reject the backup at
  the protocol upgrade and are not supported.
- **Windows** on the backup host, with the WebView2 runtime. Current Windows has it
  preinstalled. The full installer can bundle it for air-gapped machines.
- A **PBS API token** with `Datastore.Backup` on the target datastore (and
  namespace, if you use one).
- For SQL Server:
  - **TCP/IP enabled** on the instance. The client uses TDS over TCP.
  - The connecting login in the **`sysadmin`** server role. VDI backup and restore
    require it.

## 1. Prepare the PBS datastore and token

1. Create or choose a datastore on PBS.
2. Create an API token for pbsgui. Give it `Datastore.Backup` on the datastore.
   Use a namespace if you want to keep pbsgui's snapshots separate.
3. Note the PBS host, port (8007), datastore, the token id, the token secret, and
   the server's TLS fingerprint (SHA-256). pbsgui pins the fingerprint.

## 2. Prepare the SQL Server instance

Do this once per instance.

1. **Enable TCP/IP.** In SQL Server Configuration Manager, enable TCP/IP under the
   instance's protocols. Set the IPAll TCP Port (for example 1433), clear any
   dynamic port, and restart the SQL Server service. pbsgui flags a disabled
   TCP/IP during discovery and the per-instance Check reports the fix.

2. **Grant the service identity `sysadmin`.** The engine connects as its service
   identity, `NT AUTHORITY\SYSTEM`. VDI needs that login in `sysadmin`. On many
   installs it already is. If not:

   ```sql
   CREATE LOGIN [NT AUTHORITY\SYSTEM] FROM WINDOWS;
   ALTER SERVER ROLE sysadmin ADD MEMBER [NT AUTHORITY\SYSTEM];
   ```

   To use a SQL login or a Windows account instead, use **Add connection** (or
   **Edit** on a saved one) in the SQL Servers tab and pick it under
   Authentication. A Windows account is entered as `DOMAIN\user` and signs in with
   NTLM. That login needs `sysadmin` too. **Test** runs the readiness checks with
   those credentials.

## 3. Install pbsgui

1. Download an installer from the
   [releases page](https://github.com/sol1/pbsgui/releases). Pick one:
   - `pbsgui_<ver>_x64-setup.exe` bundles WebView2 and installs offline. Use it on
     air-gapped hosts.
   - `pbsgui_<ver>_x64-setup-online.exe` downloads WebView2 at install if missing.
2. Run it. The installer and binaries are code-signed by Sol1 Pty Ltd (from
   v0.3.0-rc.5). Releases before that are unsigned, so SmartScreen may warn on
   them. Choose **More info**, then **Run anyway**.
3. The installer registers and starts the `pbsgui-engine` service (LocalSystem) and
   opens the app.

Leave the service running as LocalSystem. Its configuration is signed with a key
held by the service account, so a service running as another account cannot read
it. pbsgui then shows an error and leaves the files alone; switch the account back
to recover. For Windows authentication to a SQL Server on another machine, either
give the connection a Windows account, or add the computer account
(`DOMAIN\HOSTNAME$`), which the service connects as, as a SQL Server login with
`sysadmin`. Do not change the service account.

The service runs scheduled jobs whether or not the GUI is open. The GUI requests
admin rights at launch (a UAC prompt) so it can reach the engine's control socket.

## 4. Configure and run the first backup

1. In the app, open the **PBS servers** tab. Add the server from step 1. The
   repository field is `user@realm!tokenname@host:8007:datastore[/namespace]`.
   Paste the token secret and the fingerprint. Click Test. The indicator turns
   green when it reaches the datastore and the token has `Datastore.Backup`.
2. Open the **SQL Servers** tab. Click Discover to list local instances. Probe one
   to read its databases. Use Check to confirm TCP/IP and `sysadmin`.
3. Save a SQL connection for the instance.
4. Create a job. Pick the SQL source and its databases, a protection plan, and the
   PBS server as the destination. Run it.
5. Confirm the snapshot lands in PBS and the job reports success.

For transaction-log management and the protection plans, see the SQL sections of
[DEVELOPERS.md](DEVELOPERS.md) and [STATUS.md](STATUS.md).

## Always On and Failover Cluster

Install the engine on every replica or node. It coordinates through SQL Server with
no link between the pbsgui instances. An Always On database is backed up only on the
preferred backup replica. A Failover Cluster Instance is skipped on whichever node
is not active. Give each replica's job the same backup id. It defaults to the
Availability Group name, so all nodes write one continuous chain.

Use the same PBS API token on every replica or node. PBS makes the token that first
writes a snapshot group its owner, and refuses backups into that group from any
other token. With a different token per node, the first backup after a failover
fails.

## Upgrades

Run the new installer over the old one. It restarts the service and keeps jobs,
connections, servers, settings, and secrets.

## Where things live

- Config and job store: `C:\ProgramData\pbsgui`. The folder ACL is restricted to
  SYSTEM and Administrators.
- Secrets (PBS token, SQL passwords, encryption keys): Windows Credential Manager,
  under the service (LocalSystem) account.

## Remote and fleet backups

To back up a SQL Server without running the backup CPU on it, or to back up many
SQL Servers through one machine, use relay mode. See [RELAY.md](RELAY.md).
