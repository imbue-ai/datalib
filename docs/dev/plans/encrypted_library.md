# An encrypted library, unlocked with Touch ID

**Status: proposal (2026-11-13). Nothing is built. §3 holds the
measurements from a first round of experiments on one Mac; §6 lists what
is still to be measured before building.**

## 1. The problem

A library is a folder of ordinary files. FileVault encrypts the Mac's
disk, but only the disk: every copy of the library made while the Mac
is running is in the clear. A library kept in a Dropbox or iCloud folder
is uploaded as readable files. A Time Machine backup or a copy on an
external drive holds readable files. And while you are logged in, the
library is readable whether or not Datalib is running.

The proposal: a checkbox, **Encrypt this library**, when a library is
made. Datalib then keeps the library inside an encrypted disk image.
Opening the library asks for your fingerprint. Closing it locks it again.
Nothing else about using Datalib changes.

## 2. What macOS offers

**An encrypted sparse bundle** is a disk image that holds a real APFS
filesystem, encrypted with AES-256. On disk it is a folder of 8 MiB
encrypted files called *bands*. It grows as it fills, up to a size set
when it is made; that size is only a ceiling and costs nothing until it
is used. `hdiutil` makes one, attaches it (makes its filesystem
available as a folder) and detaches it, all without admin rights:

```sh
hdiutil create -type SPARSEBUNDLE -fs APFS -encryption AES-256 -stdinpass -size 2t -volname Datalib lib.sparsebundle
hdiutil attach -stdinpass -nobrowse -owners on -mountpoint <folder> lib.sparsebundle
hdiutil detach <folder>
```

`-nobrowse` keeps the volume out of Finder's sidebar and off the
desktop. `-mountpoint` attaches it at a folder we choose, not under
`/Volumes`. `-stdinpass` reads the passphrase from a pipe, so it is never
on a command line.

**The Secure Enclave** is the Mac's security chip. A program can make a
key inside it that never leaves the chip and that the chip will use only
after Touch ID (or, depending on how the key was made, the login
password). CryptoKit's `SecureEnclave.P256.KeyAgreement.PrivateKey`
makes one. What the program gets back is a *key blob*: the key,
encrypted by the chip so that only this chip can use it. The blob can be
stored in any file; it is useless on another Mac.

Together they give Touch ID unlock:

1. At setup, make a random passphrase and create the bundle with it.
2. Make a Secure Enclave key that requires Touch ID.
3. Encrypt the passphrase to that key (ECDH with a throwaway key, HKDF,
   AES-GCM) and store the result next to the bundle.
4. To open the library, ask the chip to decrypt the passphrase. macOS
   shows the Touch ID prompt. The passphrase goes straight into
   `hdiutil attach -stdinpass`.

## 3. What was measured

Measured on an M4 Mac on macOS 26.7.2, with a throwaway Swift program
(about 80 lines, CryptoKit and LocalAuthentication, ad-hoc signed by the
linker) and TNG data only.

| question | result |
|---|---|
| Make an encrypted bundle without admin rights | Works; about 2 s for a 200 GB ceiling, 28 MB on disk when empty |
| Attach it hidden, at a folder we choose | Works; about 0.6 s. The volume reports itself as an external, removable disk image |
| Is the data encrypted on disk? | A file of 2,000 marker lines written to the volume: the marker is found in no band after detach. The same file outside the volume: found |
| A wrong passphrase | `hdiutil: attach failed - Authentication error` |
| A Secure Enclave key that needs Touch ID, from an unsigned program | Works: making the key asks nothing; using it shows the Touch ID prompt, and the bundle attached |
| What the prompt says | The program's name, then our sentence: "**seunlock** is trying to *unlock your Datalib library*". The first part is the asking program's name; the rest is ours |
| Detach while a file on the volume is open | Refused: `Resource busy`, exit 16. Detached once the file was closed |
| The TNG fixture sync (`run_sync_pipeline.py`, opt build, 54 stores, about 25 MB), three runs each, alternating | Plain folder 6.5, 5.3, 5.8 s; inside the bundle 6.7, 6.2, 6.5 s. About 10% slower. Every run exited 0 |
| Space after three syncs, each into a fresh folder deleted before the next | Bundle on disk: 162 MB, for 25 MB of live data |
| Delete everything, detach, `hdiutil compact` | Reclaimed 8.9 MB; 153 MB stayed on disk for an empty volume |
| Does Spotlight index the attached volume? | `mdutil -s` answered "unknown indexing state". Not settled |

## 4. The design

### What the person sees

- **New library** gets a checkbox: *Encrypt this library (asks for your
  fingerprint to open it)*. Only on macOS.
- After the library is made, Datalib shows a **recovery key** once, the
  way FileVault does, and says what it is for (§4.3). The person copies
  or prints it.
- **Opening** the library shows the Touch ID prompt. With a fingerprint,
  the library opens as any other does. With **Cancel**, the libraries
  screen stays up and says the library is still locked.
- **Closing** the library, switching to another, or quitting Datalib
  locks it again.
- The libraries screen marks an encrypted library with a lock, and lists
  it whether it is locked or open.

### On disk

```
~/Datalib/<name>/
  library.sparsebundle/   the encrypted volume
  unlock.json             the key blob, the throwaway public key and the
                          encrypted passphrase; no secret in the clear
  data/                   an empty folder; while unlocked, the volume is
                          attached here and this is the data root
```

The launcher's `is_data_root` (`datalib/tauri/src/launcher.rs`) knows a
library by `config.toml` or `system/`. An encrypted one is known instead
by `library.sparsebundle` beside `unlock.json`, so it is listed while
locked. The backend never learns any of this: `datalib-http` is started
with `data/` as its root once the volume is attached there.

### Opening and closing, in the shell

All of it is in the Tauri shell (`datalib/tauri/src/`), as the protected
library is. The decisions go in a pure module beside `protected.rs`
(which files make an encrypted library, the arguments to each `hdiutil`
call, what a given exit code means); `main.rs` runs them.

- **Open**: if `data/` already has the volume attached (a crash left it
  so), use it. Otherwise unlock with Touch ID, attach, then start the
  server.
- **Close**: stop the server and wait for its steps to exit, then
  detach. A refused detach (`Resource busy`) means something still has
  a file open: say which library and retry; never `-force`, which can
  cut off a doltlite write.
- **Quit** closes first.

The Touch ID part needs CryptoKit, which is Swift. Either a small Swift
helper shipped in the app bundle (as the test program was), or the
Security framework's C API from Rust (`SecKeyCreateRandomKey` with
`kSecAttrTokenIDSecureEnclave`, then `SecKeyCopyKeyExchangeResult`).
The second keeps the build Bazel-and-Rust only; it needs its own
experiment (§6).

### The recovery key

The Secure Enclave key belongs to one Mac's chip. If the Mac is
replaced, or the library is restored from a backup onto another Mac,
Touch ID cannot open it. The random passphrase itself is the recovery
key: shown once, and typed into a "Unlock with recovery key" field on
the libraries screen. After a recovery unlock on a new Mac, Datalib
makes a new Secure Enclave key there and rewrites `unlock.json`.

### Which Touch ID rule

A Secure Enclave key is made with one of these rules:

| rule | Touch ID | without Touch ID | after adding a fingerprint |
|---|---|---|---|
| `.userPresence` | yes | login password | still works |
| `.biometryAny` | yes | refused | still works |
| `.biometryCurrentSet` | yes | refused | refused: recovery key needed |

**`.userPresence`** is the proposal: a Mac mini or an iMac without a
Touch ID keyboard can use it, and a new finger does not lock anyone out.
The first round tested `.biometryAny`.

### Making an existing library encrypted

Copy it into a new bundle while it is closed, open the copy, then offer
to move the original to the Trash. It needs free space for a second
copy; the screen says how much before starting.

### Deleting one

Deleting the folder deletes everything: the key blob is in
`unlock.json` and nowhere else, so nothing is left in the keychain. The
Trash is safe here, unlike for a protected library: what goes there is
encrypted.

## 5. What it protects, and what it does not

**It protects** every copy of the library made while it is locked: a
cloud folder, a backup, a stolen drive. It also protects the library on
this Mac while Datalib is not running.

**It does not protect** the library while it is open. The attached
volume is an ordinary folder to every program running as you, an AI
agent's shell included. Combined with a protected library (§6), macOS
may still ask before letting another app in; that is not measured.

**Any program can ask the chip to unlock**, since the key blob is in a
plain file and the test program needed no entitlement to use it. What
stops it is the prompt: you see it and must touch the sensor. The name
on the prompt is the asking program's own, so a hostile program can call
itself "Datalib". The prompt is only as good as the person's habit of
touching it only when they just opened a library.

**Linux and Docker** have nothing equivalent without root (LUKS) or FUSE
(gocryptfs). The checkbox is macOS only.

## 6. What to measure before building

- **Inside the protected folder.** Put the bundle and its mount point in
  the group container, open it from the Imbue-signed app, then try to
  list the attached volume from a Terminal without Full Disk Access.
  Does macOS ask, as it does for the container itself? The volume calls
  itself a removable disk, so macOS's separate "Removable Volumes"
  permission may apply instead.
- **A large library.** Time a real library's sync in and out of a
  bundle, on the opt build (`bazel-out/…-opt/bin`), quoting only counts
  and times. Ten percent on 25 MB says little about 50 GB of mail.
- **Space.** How large does a real library's bundle get over a month of
  syncs, against its live size? Does an HFS+ bundle compact better, and
  does doltlite care which filesystem it is on?
- **Spotlight.** Whether the volume is indexed, read machine-wide
  (`mdfind 'kMDItemFSName == "*.doltlite_db"'`), not with `-onlyin`.
  If it is, turn indexing off at setup.
- **The Security framework from Rust**, to see whether the Swift helper
  can be dropped.
- **A bundle in Dropbox.** Whether the band files sync cleanly while the
  volume is detached, and what happens if two Macs open it. The answer
  to the second is probably "it breaks", which the shell would have to
  prevent with a lock file beside the bundle.
- **Sleep and screen lock.** Whether to lock the library when the screen
  locks. A detach refused while a sync runs means this can only be "lock
  when idle".
