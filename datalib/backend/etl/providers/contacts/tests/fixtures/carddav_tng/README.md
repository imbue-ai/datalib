# carddav_tng — TNG address books, one file per flavor

Each file is written the way one real service writes vCards, so the
parse and render paths meet every shape we have seen. The shapes were
read off a live Fastmail account and real Google and Fastmail exports
(September 2026); only the crew is made up.

| file | flavor | what it carries |
|---|---|---|
| `Bridge.vcf` | Fastmail over CardDAV | CRLF, lines folded at 75 octets (the photos run across several); `PROP-ID` on every property; `TYPE=WORK,PREF;PREF=1`; `o1.ORG`; `FN;DERIVED=TRUE`; basic-format `CREATED`, `REV` and `BDAY`; `X-ADDRESSBOOKSERVER-KIND`; and a contact group, *Senior Staff*, whose `X-ADDRESSBOOKSERVER-MEMBER`s name Picard and Riker by `urn:uuid:<UID>`. |
| `Engineering.vcf` | Fastmail's export | The same properties with LF line ends and no groups: an export leaves them out. The note is folded mid-sentence, just before a space. |
| `Borg.vcf` | Google Contacts' export | CRLF, no `UID`, `REV` or `PRODID`; `item1.EMAIL` paired with a blank `item1.X-ABLabel`, a phone named by a typed one (`Subspace relay`); `TYPE=INTERNET;TYPE=WORK` repeated; `CATEGORIES:myContacts`; and a drone with no name at all, only an address. |

`carddav_playback.rs` serves `Bridge.vcf`'s cards over a Fastmail-shaped
CardDAV exchange, so a change here is a change to what that test syncs.
`../carddav_tng_v2/` is these books one sync later.
