# GlobalPlatformPro captures of the J3R452 under SCP03

`gp/test/transcripts.test.ts` replays these logs through cardkit: the handshake
must prove the keys, and every command gp sent under the C-MAC must be the one
cardkit sends, byte for byte. While a file is missing its test is skipped with
`pending fixtures`; once it is here the test is required.

They are cashucard's (`tools/cardkit/gp/test/fixtures` at 0229dc8, its public
repository), copied byte for byte with the section below. Taken by the owner
only. No agent runs gp against a card, and no agent writes a fixture.

This repository has none of cashucard's H0 captures of the J3R180.
`gp/test/scp02-pins.test.ts` pins the bytes cardkit sends that chip instead.

## The J3R452 under SCP03

The `scp03-*.txt` logs are the owner's captures of a second card, taken on
2026-11-08: a Dangerous Things J3R452 (NXP P71D600, JCOP 4.5, IC serial number
36799121, in the CPLC) in the same ACR122U, through GlobalPlatformPro
v25.10.20. That card opens SCP03 with i=10 (cashucard's D181). Before these
were taken its shipped key set was replaced with the public default `40` to
`4F` at key version 01, so nothing in them is secret and every session key,
cryptogram and C-MAC can be recomputed. The first line of each is the command
that made it; the one local path was reduced to its file name.

| Log                                      | What gp did                                                                 |
| ---------------------------------------- | --------------------------------------------------------------------------- |
| `scp03-07-list-default-keyver1.txt`      | `--list`, INITIALIZE UPDATE naming key version 01 (`--key-ver 1`)           |
| `scp03-08-list-default-auto.txt`         | `--list`, INITIALIZE UPDATE letting the card choose (P1 00)                 |
| `scp03-09-install-probe-default-key.txt` | `--install` of a probe package: INSTALL [for load], 7 LOAD blocks, INSTALL  |
| `scp03-10-delete-probe-default-key.txt`  | `--delete -f` of the probe package: DELETE with P2 80, its instance with it |

`transcripts.test.ts` replays all four under the public default: the challenge
and both cryptograms, every session key against node:crypto, all 49 commands
under the C-MAC byte for byte, the probe's INSTALLs and LOAD blocks against
this module's builders and the load file's sha256 (`PROBE_LOAD_FILE_SHA256`),
and each login's shape. The probe is this repository's `tools/j3r180-probe`
(package `F0464D50524F42`), not the card's applet, so only a new capture moves
its hash. The owner's earlier captures of this card under the seller's key set
are not here and never will be: they serve shapes only, and they were made
under keys no test may hold.

It also reads `scp03-07`'s GET STATUS as `reset.ts` reads it, with the reset's
own commands, which are gp's byte for byte: the seller's four applets (the NDEF
tag at `D2760000850101`, made from `D276000085`, among them) and twelve load
files, none of them this card's. The fake card's `sellerApplets` layout holds
the same four applets. Fedimint Card's AIDs (`F0464D43415244` and its two
instances) collide with none of them, so the install's sweep and the reset
never meet the seller's objects.

No capture of first contact is here: the one the owner made was under the
seller's key set. First contact (PUT KEY P1 00 at version FF, cashucard's
D182) rests on the fake card until it is traced on silicon.
