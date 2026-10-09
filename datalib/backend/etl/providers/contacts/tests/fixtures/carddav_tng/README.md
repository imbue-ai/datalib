# carddav_tng — TNG address books, one file per flavor

Each file is written the way one real service writes vCards, so the
parse and render paths meet every shape we have seen. The shapes were
read off a live Fastmail account and real Google and Fastmail exports;
only the crew is made up.

| file | flavor | what it carries |
|---|---|---|
| `Bridge.vcf` | Fastmail over CardDAV | CRLF, lines folded at 75 octets (the photos run across several); `PROP-ID` on every property; `TYPE=WORK,PREF;PREF=1`; `o1.ORG`; `FN;DERIVED=TRUE`; basic-format `CREATED`, `REV` and `BDAY`; `X-ADDRESSBOOKSERVER-KIND`; and a contact group, *Senior Staff*, whose `X-ADDRESSBOOKSERVER-MEMBER`s name Picard and Riker by `urn:uuid:<UID>`. |
| `Engineering.vcf` | Fastmail's export | The same properties with LF line ends and no groups: an export leaves them out. The note is folded mid-sentence, just before a space. |
| `Borg.vcf` | Google Contacts' export | CRLF, no `UID`, `REV` or `PRODID`; `item1.EMAIL` paired with a blank `item1.X-ABLabel`, a phone named by a typed one (`Subspace relay`); `TYPE=INTERNET;TYPE=WORK` repeated; `CATEGORIES:myContacts`; and a drone with no name at all, only an address. |
| `Maquis.vcf` | a plain vCard 3.0 file | Two cards and nothing unusual. It is here to be deleted: `../carddav_tng_v2/` does not have it. |

`../carddav_tng_v2/` is these books one sync later.
`carddav_playback.rs` serves `Bridge.vcf`'s cards, then v2's, over a
Fastmail-shaped CardDAV exchange, so a change here is a change to what
that test syncs.

## The photos

The photos on the cards are the actors who played each part, from
Wikimedia Commons, cropped to a 96-pixel square and inlined as JPEG.
Each keeps its own licence, not the repo's MIT; the cropped Forbes
photo is CC BY-SA 2.0 like its original. Kalita has none: Commons has
no free photo of Shannon Cochran. The Borg cards have none because a
Google export carries a photo only as a link.

| card | photo | by | licence |
|---|---|---|---|
| Jean-Luc Picard | [Patrick Stewart](https://commons.wikimedia.org/wiki/File:PatrickStewart2004-08-03.jpg) | Cdt. Patrick Caughey | public domain |
| William T. Riker | [Jonathan Frakes](https://commons.wikimedia.org/wiki/File:Jonathan_Frakes_(cropped).jpg) | Benjamin Krahl | public domain |
| Data | [Brent Spiner](https://commons.wikimedia.org/wiki/File:Brent_Spiner_2016.jpg) | Florida Supercon | [CC BY 2.0](https://creativecommons.org/licenses/by/2.0) |
| Worf (in `../carddav_tng_v2/`) | [Michael Dorn](https://commons.wikimedia.org/wiki/File:Michael_Dorn_head_3.jpg) | Canonblack | public domain |
| Geordi La Forge | [LeVar Burton](https://commons.wikimedia.org/wiki/File:LeVar_Burton_(32468569868).jpg) | Super Festivals | [CC BY 2.0](https://creativecommons.org/licenses/by/2.0) |
| Ro Laren | [Michelle Forbes](https://commons.wikimedia.org/wiki/File:Michelle_Forbes_2009a_Comic-Con.jpg) | sookiebontemps | [CC BY-SA 2.0](https://creativecommons.org/licenses/by-sa/2.0) |
