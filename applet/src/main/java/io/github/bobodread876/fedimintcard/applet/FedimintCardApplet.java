package io.github.bobodread876.fedimintcard.applet;

import javacard.framework.APDU;
import javacard.framework.Applet;
import javacard.framework.CardRuntimeException;
import javacard.framework.ISO7816;
import javacard.framework.ISOException;
import javacard.framework.SystemException;
import javacard.framework.JCSystem;
import javacard.framework.OwnerPIN;
import javacard.framework.Util;
import javacard.security.AESKey;
import javacard.security.ECPrivateKey;
import javacard.security.ECPublicKey;
import javacard.security.KeyBuilder;
import javacard.security.KeyPair;
import javacard.security.MessageDigest;
import javacard.security.RSAPrivateKey;
import javacard.security.Signature;
import javacardx.crypto.Cipher;

/**
 * Fedimint spend-key applet. Holds up to {@link #SLOTS} secp256k1 keys, one per Fedimint
 * note, and signs a note's spend with BIP340 Schnorr once a PIN has been given. No command ever
 * returns a private key.
 *
 * <p>3.0 (docs/OFFLINE-PLAN.md) makes the card safe to pay with offline. Each slot signs one
 * transaction, ever: SIGN records the hash in the same commit that marks the slot spent, before
 * the signature leaves the card, and afterwards answers only that hash again (a lost answer).
 * So a genuine card cannot spend a note twice, at any till, online or not. The card also keeps
 * each note's amount and federation signature (SET_NOTE) so a till with no network can check
 * the note, the opening of change it is owed (SET_CHANGE), a daily limit on a clock that only
 * Bitcoin block headers move (SET_LIMIT, SET_HEADER), and a P-256 attestation key that the
 * registry certifies at provisioning (ATTEST). The burn-then-answer order, the header clock and
 * attestation follow cashucard's applet (MIT, see NOTICE).
 *
 * <pre>
 * CLA B0
 * 10 GEN_KEY     P1=slot P2=0|1 Lc=0  -> 32 bytes x-only pubkey. P2=1 re-keys a KEYED or SPENT slot (after VERIFY_PIN once a
 *                                        PIN is set); a slot holding a note or owed change is never re-keyed (6985),
 *                                        nor slot 0 once SET_CERT has taken a certificate (6985)
 * 11 GET_PUB     P1=slot Lc=0         -> 32 bytes x-only pubkey
 * 12 SIGN        P1=slot Lc=32 (hash) -> 64 bytes R.x || s (after VERIFY_PIN). A NOTE slot: counted against the day's limit,
 *                                        then spent. A SPENT slot: the same hash only, answered with the signature it
 *                                        gave. Else 6985; over the limit 6A95
 * 13 STATE_KEY   P1=P2=0              -> 32 bytes (after VERIFY_PIN)
 * 14 SET_NOTE    P1=slot Lc=56+8..72  amount (8, msat, big-endian) || federation signature (48) || the registry's
 *                                        ECDSA-SHA256 (DER) over "fedimintcard/note1" || the slot's x-only key || the
 *                                        56 bytes. A KEYED slot, or a CHANGE slot finished (same amount); the slot
 *                                        becomes NOTE. A bad signature 6982, no registry key 6985: nothing written
 * 15 GET_SLOT    P1=slot              -> state (1) || x-only pubkey (32) || amount and signature (56) || spent hash (32)
 *                                        || opening (66)
 * 16 GET_STATE                        -> slot states (32) || now (4) || window (4) || limit (8) || spent today (8)
 *                                        || cert length (1) || attest made (1) || registry key set (1) || PIN state (1)
 *                                        || wrong tries in a row (1) || askings owed (2) || anchored in this selection (1)
 *                                        || signer (1: bit 0 the chip's RSA path, bit 1 the nonce key in RAM)
 * 17 SET_CHANGE  P1=slot Lc=74        amount (8) || txid (32) || output index (2) || blinding key (32). A KEYED slot becomes CHANGE.
 *                                        After SET_PIN, only with this selection's PIN (6A94)
 * 18 SET_LIMIT   Lc=8                 the day's limit in msat (0: none). Only before SET_PIN
 * 19 SET_HEADER  Lc=80                a Bitcoin block header -> now (4). Moves the clock if later, with enough work and
 *                                        not weak (6A93, nothing written). Before SET_PIN on a card with no value it is
 *                                        an anchor: the clock at its time, the day's window and spent cleared
 * 30 ATTEST_INIT                      -> P-256 public key (65). Makes the PIN key, then the attestation key; only before SET_PIN
 *                                        and the certificate
 * 31 SET_CERT    Lc=1..160            the registry's certificate over this card; once, before SET_PIN
 * 32 GET_CERT                         -> the certificate
 * 33 ATTEST      Lc=32 (challenge)    -> ECDSA-SHA256 (DER) over "fedimintcard/at1" || challenge || slot 0's x-only pubkey
 * 34 SET_REGISTRY_KEY Lc=65           the registry's P-256 certificate key (uncompressed); only before SET_PIN
 * 35 RELEASE     P1=slot Lc=32+DER    txid (32) || the registry's ECDSA-SHA256 (DER) over "fedimintcard/rel1" || slot 0's
 *                                        x-only key || slot (1) || txid. A SPENT slot whose one transaction is txid goes
 *                                        back to NOTE; a CHANGE slot owed by txid goes back to KEYED. -> the new state (1).
 *                                        The registry signs one only for a transaction the federation refused, or whose
 *                                        report it refused as a double spend (docs/OFFLINE-PROTOCOL.md § 6)
 * 40 VERIFY_PIN  P1=01 P2=0 Lc=90     a sealed PIN block -> the session may SIGN. After wrong tries the next try first owes
 *                                        a wait, served by sending the same VERIFY_PIN again while it answers 6A98. No live
 *                                        nonce 6985; an envelope that does not open is a wrong try (63CX)
 * 41 SET_PIN     P1=01 P2=0 Lc=90     a sealed PIN block -> once per card, after an anchor in the same selection (6A96), on
 *                                        a card with the registry key and no value, under a live nonce (6985); an envelope
 *                                        that does not open 6700, nothing written. The selection that sent it counts as
 *                                        having the PIN
 * 42 CHANGE_PIN  P1=01 P2=0 Lc=99     a sealed old PIN block || new PIN block. After VERIFY_PIN; closes the session. A wrong
 *                                        old PIN, or an envelope that does not open, counts as a try
 * 44 GET_NONCE   P1=01 P2=0           -> nonce (16) || PIN key (33, compressed) || binding (DER, 8..72): ECDSA-SHA256 by
 *                                        the attestation key over "fedimintcard/pin1" || the PIN key. After SET_CERT (6985).
 *                                        No PIN. The nonce opens one sealed PIN command
 * </pre>
 *
 * <p>The PIN is sealed (docs/OFFLINE-PROTOCOL.md § 9, Cashu Card 1.9's sealed PIN, MIT, see NOTICE): every PIN
 * command is sealed. VERIFY_PIN, SET_PIN and CHANGE_PIN take only P1 = 01, any other P1 or P2 (00 among
 * them) 6B00. Their data is an envelope, E (65, the sender's secp256k1 key for this message) || ct ||
 * tag (16), sealed to the card's PIN key under the nonce of the last GET_NONCE: x the X of the point E and the PIN key
 * share, block(i, m) = SHA-256("fedimintcard/seal1" || i || x || E || nonce || INS || m), the tag the first 16 bytes of
 * block(0, ct), the keystream blocks 1, 2, ... The clear data is a PIN block, the PIN's length (1) || the PIN || zeros
 * to 8 bytes. The nonce is RAM and opens at most one envelope: the first sealed command to reach its opening spends
 * it, opened or not; a GET_NONCE replaces it and a SELECT ends it. An asking of the wait does not spend it.
 *
 * <p>Slot states: EMPTY 0, KEYED 1 (a key, no note), NOTE 2, CHANGE 3 (a key owed change), SPENT 4.
 * The day's limit counts what a note signs away whole: the card cannot see a transaction's
 * outputs, so change coming back is not netted off (cashucard nets it; its card makes its own
 * change output). Amounts are the terminal's word, as in cashucard: the card cannot check a BLS
 * signature, the till does.
 *
 * <p>PIN status words: {@code 69 84} no PIN set yet, {@code 6A 94} a PIN is set and this
 * session has not verified it, {@code 63 CX} a wrong PIN, X the wrong tries in a row (F for 15
 * or more), {@code 6A 98} keep holding: a wait is owed before the next try. {@code SET_PIN} on a
 * card that already has a PIN answers {@code 69 85}. The PIN never blocks: no command answers
 * {@code 69 83}.
 *
 * <p>The wait: after the n-th wrong try in a row the next try first owes 2^(n-1) askings, at most
 * {@link #PIN_OWED_MOST}. An asking is a VERIFY_PIN sent while a wait is owed: the card does a
 * signature's work with {@code workKeys}, a key that holds no value, and answers {@code 6A98}.
 * Askings come in stretches of {@link #PIN_STRETCH}; the asking after a stretch writes what is
 * still owed. Any other command, SELECT included, starts the stretch again. The count and the
 * wait ({@code pinMarks}) are written before the PIN is compared, so a try torn anywhere was
 * either counted with its wait or never made.
 *
 * <p>{@code SELECT} answers two data bytes with {@code 90 00}: major version, then
 * minor. This is the applet's version alone — no card-manager status byte, no
 * FCI — so the first byte of the answer is the version. See {@link #VERSION_MAJOR}.
 *
 * <p>The verified flag is {@code CLEAR_ON_DESELECT} transient: a session is one
 * applet selection, and a card reset or a SELECT of another AID ends it. The PIN
 * itself lives in {@link OwnerPIN}, whose own counter is filled again before every try, so it
 * never runs out: the wrong tries are counted in {@code pinMarks}.
 */
public final class FedimintCardApplet extends Applet {

    static final byte CLA = (byte) 0xB0;
    static final byte INS_GEN_KEY = (byte) 0x10;
    static final byte INS_GET_PUB = (byte) 0x11;
    static final byte INS_SIGN = (byte) 0x12;
    static final byte INS_STATE_KEY = (byte) 0x13;
    static final byte INS_VERIFY_PIN = (byte) 0x40;
    static final byte INS_SET_PIN = (byte) 0x41;
    static final byte INS_CHANGE_PIN = (byte) 0x42;
    static final byte INS_GET_NONCE = (byte) 0x44;
    static final short SLOTS = 32;

    /** SELECT answers this major and {@link #VERSION_MINOR}; see the class javadoc. */
    static final byte VERSION_MAJOR = 0x04;
    static final byte VERSION_MINOR = 0x00;

    /** PIN size bounds (bytes; the applet never interprets them), and OwnerPIN's own counter, filled again before every try. */
    static final byte PIN_MIN = 4;
    static final byte PIN_MAX = 8;
    static final byte PIN_TRIES = 3;

    /** The sealed form's P1, the only one the PIN commands take. */
    static final byte SEALED = 0x01;
    /** A sealed PIN command's Lc: E (65) || the sealed PIN block (9) || tag (16); CHANGE_PIN's carries two blocks. */
    static final short SEAL_LC = 90;
    static final short SEAL_LC_CHANGE = 99;
    static final short E_LEN = 65;
    static final short TAG_LEN = 16;
    static final short PIN_BLOCK = 9;
    static final short NONCE_LEN = 16;
    /**
     * The curve check's 128 bytes of field room in the APDU buffer: past the longest envelope (5 + 99 = 104), and below
     * 240, the highest byte any command already uses (SET_NOTE's message). A buffer shorter than that is refused (6F00)
     * before anything is written past it.
     */
    static final short FIELD = 112;
    static final short FIELD_END = 240;

    /** Keep holding: a wait is owed before the next PIN try; send the same VERIFY_PIN again. */
    static final short SW_KEEP_HOLDING = 0x6A98;
    /** The askings of one stretch of the PIN's wait (about 8 s on a J3R180). */
    static final short PIN_STRETCH = 16;
    /** The most askings owed before one try: the wait after the fifteenth wrong try in a row and every later one. */
    static final short PIN_OWED_MOST = 0x4000;
    /** The wrong tries in a row stop counting here; 63CF reports 15 or more. */
    static final byte PIN_WRONG_MOST = 15;
    /**
     * SHA-256 rounds over 32 bytes in RAM that stand for one asking's signature on a chip whose
     * signer keeps its nonce key in persistent memory (so a signature would write flash). C1
     * times both on the chips and sets this to match the signature's half second.
     */
    static final short WORK_HASHES = 512;
    static final short SW_PIN_NOT_SET = 0x6984;
    static final short SW_PIN_NEEDED = 0x6A94;
    static final short SW_OVER_LIMIT = 0x6A95;
    static final short SW_LITTLE_WORK = 0x6A93;
    static final short SW_NOT_ANCHORED = 0x6A96;

    static final byte INS_SET_NOTE = (byte) 0x14;
    static final byte INS_GET_SLOT = (byte) 0x15;
    static final byte INS_GET_STATE = (byte) 0x16;
    static final byte INS_SET_CHANGE = (byte) 0x17;
    static final byte INS_SET_LIMIT = (byte) 0x18;
    static final byte INS_SET_HEADER = (byte) 0x19;
    static final byte INS_ATTEST_INIT = (byte) 0x30;
    static final byte INS_SET_CERT = (byte) 0x31;
    static final byte INS_GET_CERT = (byte) 0x32;
    static final byte INS_ATTEST = (byte) 0x33;
    static final byte INS_SET_REGISTRY_KEY = (byte) 0x34;
    static final byte INS_RELEASE = (byte) 0x35;

    static final byte ST_EMPTY = 0;
    static final byte ST_KEYED = 1;
    static final byte ST_NOTE = 2;
    static final byte ST_CHANGE = 3;
    static final byte ST_SPENT = 4;

    /** A note: its amount in msat (8, big-endian), then the federation's BLS signature (48). */
    static final short NOTE_LEN = 56;
    /** Change owed to a slot: the transaction (32), the output's index (2), its blinding key (32). */
    static final short OPEN_LEN = 66;
    /** Where SET_NOTE builds its 106-byte message in the APDU buffer: past its longest data (133). */
    static final short NOTE_MSG = 134;
    static final short CERT_MAX = 160;
    static final short HEADER_LEN = 80;
    static final short STATE_LEN = 65;

    /** The clock record: now (4), the day's window start (4), the hardest bits taken (4), limit (8), spent in the window (8). */
    static final short CK_NOW = 0;
    static final short CK_WINDOW = 4;
    static final short CK_BITS = 8;
    static final short CK_LIMIT = 12;
    static final short CK_SPENT = 20;
    static final short CK_LEN = 28;

    /**
     * The least work a header must show, as `bits` little-endian: 0x17087BC0, from cashucard's
     * applet. A header under it would be cheap to make, and could walk the clock forward.
     */
    private static final byte[] FLOOR_BITS = { (byte) 0xC0, (byte) 0x7B, (byte) 0x08, (byte) 0x17 };
    /** 86 400 seconds, big-endian. */
    private static final byte[] DAY_SECONDS = { (byte) 0x00, (byte) 0x01, (byte) 0x51, (byte) 0x80 };
    /** RELEASE's label: ASCII "fedimintcard/rel1", 17 bytes. */
    private static final byte[] RELEASE_LABEL = {
        'f','e','d','i','m','i','n','t','c','a','r','d','/','r','e','l','1' };
    /** SET_NOTE's label: ASCII "fedimintcard/note1", 18 bytes. */
    private static final byte[] NOTE_LABEL = {
        'f','e','d','i','m','i','n','t','c','a','r','d','/','n','o','t','e','1' };
    /** ATTEST's label: ASCII "fedimintcard/at1", 16 bytes. */
    private static final byte[] ATTEST_LABEL = {
        'f','e','d','i','m','i','n','t','c','a','r','d','/','a','t','1' };
    /** The seal's label: ASCII "fedimintcard/seal1", 18 bytes (Cashu's "CashuCard/seal"). */
    private static final byte[] SEAL_LABEL = {
        'f','e','d','i','m','i','n','t','c','a','r','d','/','s','e','a','l','1' };
    /** The PIN key's binding: ASCII "fedimintcard/pin1", 17 bytes (Cashu's "CashuCard/pinkey"). */
    private static final byte[] PIN_LABEL = {
        'f','e','d','i','m','i','n','t','c','a','r','d','/','p','i','n','1' };

    /**
     * The field's arithmetic for the curve check, by the chip's RSA (Cashu's p*p path, its applet's RSA_PP): the
     * modulus p*p, 64 bytes, and the exponents 3 (x^3), (p-1)/2 (Euler's test) and (p+1)/4 + (p-1)/2 (the root). Each
     * answer is taken mod p after (reduceModP).
     */
    private static final byte[] RSA_PP = {
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x01,
        (byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,
        (byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFE,(byte)0xFF,(byte)0xFF,(byte)0xFC,(byte)0x2F,
        (byte)0x00,(byte)0x00,(byte)0x03,(byte)0xCF,(byte)0xFF,(byte)0xFF,(byte)0xFC,(byte)0x2E,
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFE,
        (byte)0xFF,(byte)0xFF,(byte)0xFC,(byte)0x2F,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,
        (byte)0x00,(byte)0x00,(byte)0x00,(byte)0x01,(byte)0x00,(byte)0x00,(byte)0x03,(byte)0xD1
    };
    private static final byte[] RSA_D_CUBE = { (byte) 0x03 };
    private static final byte[] RSA_D_ROOT = {
        (byte)0xBF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0x3F,(byte)0xFF,(byte)0xFD,(byte)0x23
    };
    private static final byte[] RSA_D_EULER = {
        (byte)0x7F,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0x7F,(byte)0xFF,(byte)0xFE,(byte)0x17
    };

    // P-256, for the attestation key (native ECDSA on the chip, not our software Schnorr).
    private static final byte[] R1_P = {
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,0,0,0,1,0,0,0,0,0,0,0,0,0,0,0,0,
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF };
    private static final byte[] R1_A = {
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,0,0,0,1,0,0,0,0,0,0,0,0,0,0,0,0,
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFC };
    private static final byte[] R1_B = {
        (byte)0x5A,(byte)0xC6,(byte)0x35,(byte)0xD8,(byte)0xAA,(byte)0x3A,(byte)0x93,(byte)0xE7,
        (byte)0xB3,(byte)0xEB,(byte)0xBD,(byte)0x55,(byte)0x76,(byte)0x98,(byte)0x86,(byte)0xBC,
        (byte)0x65,(byte)0x1D,(byte)0x06,(byte)0xB0,(byte)0xCC,(byte)0x53,(byte)0xB0,(byte)0xF6,
        (byte)0x3B,(byte)0xCE,(byte)0x3C,(byte)0x3E,(byte)0x27,(byte)0xD2,(byte)0x60,(byte)0x4B };
    private static final byte[] R1_G = {
        (byte)0x04,
        (byte)0x6B,(byte)0x17,(byte)0xD1,(byte)0xF2,(byte)0xE1,(byte)0x2C,(byte)0x42,(byte)0x47,
        (byte)0xF8,(byte)0xBC,(byte)0xE6,(byte)0xE5,(byte)0x63,(byte)0xA4,(byte)0x40,(byte)0xF2,
        (byte)0x77,(byte)0x03,(byte)0x7D,(byte)0x81,(byte)0x2D,(byte)0xEB,(byte)0x33,(byte)0xA0,
        (byte)0xF4,(byte)0xA1,(byte)0x39,(byte)0x45,(byte)0xD8,(byte)0x98,(byte)0xC2,(byte)0x96,
        (byte)0x4F,(byte)0xE3,(byte)0x42,(byte)0xE2,(byte)0xFE,(byte)0x1A,(byte)0x7F,(byte)0x9B,
        (byte)0x8E,(byte)0xE7,(byte)0xEB,(byte)0x4A,(byte)0x7C,(byte)0x0F,(byte)0x9E,(byte)0x16,
        (byte)0x2B,(byte)0xCE,(byte)0x33,(byte)0x57,(byte)0x6B,(byte)0x31,(byte)0x5E,(byte)0xCE,
        (byte)0xCB,(byte)0xB6,(byte)0x40,(byte)0x68,(byte)0x37,(byte)0xBF,(byte)0x51,(byte)0xF5 };
    private static final byte[] R1_N = {
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,0,0,0,0,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xBC,(byte)0xE6,(byte)0xFA,(byte)0xAD,
        (byte)0xA7,(byte)0x17,(byte)0x9E,(byte)0x84,(byte)0xF3,(byte)0xB9,(byte)0xCA,(byte)0xC2,
        (byte)0xFC,(byte)0x63,(byte)0x25,(byte)0x51 };

    /**
     * The persistent PIN state, and the commit point that makes a torn PIN write harmless: the
     * byte is written after the PIN it describes, so a tear reads as not set, never as set with a
     * PIN nobody knows. There is no blocked state.
     */
    static final byte PIN_STATE_NOT_SET = 0;
    static final byte PIN_STATE_SET = 1;

    // offsets in pinMarks (persistent): the wrong tries in a row, then the askings owed (2)
    static final short PM_WRONG = 0;
    static final short PM_OWED = 1;
    private static final byte[] MARKS_CLEAR = { 0, 0, 0 };
    // offsets in held (transient, CLEAR_ON_DESELECT): this selection has the PIN, this selection
    // anchored the clock, the askings of the PIN's wait in this stretch
    static final short H_PIN_GIVEN = 0;
    static final short H_ANCHORED = 1;
    static final short H_ASKED = 2;
    // ... and whether the nonce of the last GET_NONCE is live (one opening at most)
    static final short H_NONCE = 3;

    private final ECPrivateKey[] privs = new ECPrivateKey[SLOTS];
    private final ECPublicKey[] pubs = new ECPublicKey[SLOTS];
    private final byte[] state = new byte[SLOTS];
    private final byte[] notes = new byte[(short) (SLOTS * NOTE_LEN)];
    private final byte[] spentHash = new byte[(short) (SLOTS * 32)];
    /** The one signature a spent slot gave: a repeat of its transaction is answered from here, not signed again. */
    private final byte[] spentSig = new byte[(short) (SLOTS * 64)];
    private final byte[] openings = new byte[(short) (SLOTS * OPEN_LEN)];
    private final byte[] clock = new byte[CK_LEN];
    /** The certificate: its length (1), then up to {@link #CERT_MAX} bytes. */
    private final byte[] cert = new byte[(short) (CERT_MAX + 1)];
    /** The attestation key pair, built at install; its key is made at ATTEST_INIT. */
    private final KeyPair attest;
    /** Whether ATTEST_INIT has made the attestation key. */
    private boolean attestMade;
    private final Signature ecdsa;
    /** The registry's certificate key (P-256), which signs releases; set at provisioning. */
    private final ECPublicKey registryKey;
    private boolean registryKeySet;
    private final MessageDigest sha;
    /** Working room for SIGN's limit, the header checks and ATTEST (CLEAR_ON_DESELECT). */
    private final byte[] x;
    // offsets in x
    private static final short X_WINDOW = 0;   // 4
    private static final short X_SPENT = 4;    // 8
    private static final short X_HASH = 12;    // 32, the hash SIGN is asked for
    private static final short X_NUM = 44;     // 4
    private static final short X_OUT = 48;     // 64: a header's hash, then its target
    private static final short X_FLOOR = 112;  // 32
    private static final short X_LEN = 144;
    private static final short X_REC = 0;      // 28, SET_HEADER's new clock record (an anchor)
    // x inside a sealed PIN command (unused there otherwise): the seal's room, the point the two keys share (65), a
    // block of the hash (32), the counter (1) and the INS (1); then reduceModP's 40 bytes, apart from it
    private static final short S_POINT = 0;
    private static final short S_HASH = 65;
    private static final short S_I = 97;
    private static final short S_INS = 98;
    private static final short X_RED = 100;    // 40
    private final KeyPair gen;
    private final byte[] tmp;
    /**
     * CHANGE_PIN's new half, persistent (not cleared on deselect: it is zeroed at the end of every
     * call that writes it). A transient array would be wiped by {@link #tryPin}'s cleanup, which
     * clears {@link #tmp} along with the APDU buffer before the new PIN is written.
     */
    private final byte[] newPin;
    private final SchnorrHW schnorr;
    private final OwnerPIN pin;
    private byte pinState;
    /** The wrong tries in a row and the askings owed before the next try (PM_*). */
    private final byte[] pinMarks;
    /** What this selection holds (H_*); cleared on deselect and at power loss. */
    private final byte[] held;
    /**
     * A secp256k1 key pair that holds no value, made at the first SET_PIN: the PIN's wait signs
     * with it, so no slot key ever does anything but SIGN.
     */
    private KeyPair workKeys;
    /**
     * The card secret behind STATE_KEY: 32 random bytes made at install, kept in a key object so
     * the chip protects it at rest like the slot keys. It never leaves the card; only the HMAC
     * derived from it does.
     */
    private final AESKey secret;
    /**
     * The PIN key (secp256k1): it agrees with readers' points and does nothing else (never a slot key, never the work
     * keys). Built and made at ATTEST_INIT, before the attestation key; fixed from SET_CERT for the card's life.
     */
    private KeyPair pinKey;
    /** The PIN key's binding: the DER's length (1, 0 = none yet), then the DER (up to 72); made at the first GET_NONCE after SET_CERT. */
    private final byte[] pinKeySig;
    /** The nonce of the last GET_NONCE (CLEAR_ON_DESELECT); live while held[H_NONCE] is set. */
    private final byte[] nonce;
    /** The chip's RSA for the curve check's field arithmetic, and its three powers mod p*p; built at the first ATTEST_INIT. */
    private Cipher rsa;
    private RSAPrivateKey rsaCube;
    private RSAPrivateKey rsaRoot;
    private RSAPrivateKey rsaEuler;

    private FedimintCardApplet() {
        // A slot's key objects are built at its first GEN_KEY (keysFor), not here: the J3R180
        // runs the constructor inside a bounded commit buffer and refuses INSTALL (6F00) when it
        // builds too much at once (16 slots' keys failed, 8 passed).
        gen = new KeyPair(KeyPair.ALG_EC_FP, KeyBuilder.LENGTH_EC_FP_256);
        setParams((ECPrivateKey) gen.getPrivate(), (ECPublicKey) gen.getPublic());
        tmp = JCSystem.makeTransientByteArray((short) 65, JCSystem.CLEAR_ON_DESELECT);
        // Install-time only, and persistent: see the field's comment.
        newPin = new byte[PIN_MAX];
        schnorr = new SchnorrHW();
        schnorr.init();
        // Install-time only: OwnerPIN and the state byte are the applet's persistent PIN store.
        // A new instance starts on the not-set path, as the check in set() expects.
        pin = new OwnerPIN(PIN_TRIES, PIN_MAX);
        pinState = PIN_STATE_NOT_SET;
        pinMarks = new byte[3];
        held = JCSystem.makeTransientByteArray((short) 4, JCSystem.CLEAR_ON_DESELECT);
        nonce = JCSystem.makeTransientByteArray(NONCE_LEN, JCSystem.CLEAR_ON_DESELECT);
        pinKeySig = new byte[73];
        attest = new KeyPair(KeyPair.ALG_EC_FP, KeyBuilder.LENGTH_EC_FP_256);
        p256((javacard.security.ECKey) attest.getPrivate());
        p256((javacard.security.ECKey) attest.getPublic());
        ecdsa = Signature.getInstance(Signature.ALG_ECDSA_SHA_256, false);
        registryKey = (ECPublicKey) KeyBuilder.buildKey(KeyBuilder.TYPE_EC_FP_PUBLIC, KeyBuilder.LENGTH_EC_FP_256, false);
        p256(registryKey);
        sha = MessageDigest.getInstance(MessageDigest.ALG_SHA_256, false);
        x = JCSystem.makeTransientByteArray(X_LEN, JCSystem.CLEAR_ON_DESELECT);
        secret = (AESKey) KeyBuilder.buildKey(KeyBuilder.TYPE_AES, KeyBuilder.LENGTH_AES_256, false);
        schnorr.random(tmp, (short) 0, (short) 32);
        secret.setKey(tmp, (short) 0);
        Util.arrayFillNonAtomic(tmp, (short) 0, (short) 32, (byte) 0);
    }

    public static void install(byte[] b, short o, byte l) {
        new FedimintCardApplet().register();
    }

    private static void setParams(ECPrivateKey priv, ECPublicKey pub) {
        if (priv != null) Secp256k1.curve(priv);
        if (pub != null) Secp256k1.curve(pub);
    }

    /**
     * A session is one selection. OwnerPIN keeps its validated flag in CLEAR_ON_RESET memory, so
     * left alone a verified PIN outlives a SELECT of another applet, or of this one again, until
     * the card loses power; on the J3R452 it held for a whole 17-minute run in a contact reader.
     * Both ends of a selection drop it: select() covers a re-SELECT of this applet, which a card
     * need not report as a deselect.
     */
    @Override
    public boolean select() {
        newSelection();
        return true;
    }

    @Override
    public void deselect() {
        newSelection();
    }

    /**
     * What a selection begins and ends with: no PIN given, no anchor, no stretch of the wait under
     * way, no live nonce, and nothing in the working arrays. They are CLEAR_ON_DESELECT, and are zeroed here as
     * well for a chip that keeps a same-package applet's transient memory across a SELECT.
     * {@link #newPin} is persistent and zeroed at the end of every call that writes it; a PIN
     * write torn inside the call can leave it, so it is zeroed here too, only when it is not
     * already zero (a check costs no write).
     */
    private void newSelection() {
        pin.reset();
        Util.arrayFillNonAtomic(held, (short) 0, (short) held.length, (byte) 0);
        Util.arrayFillNonAtomic(x, (short) 0, X_LEN, (byte) 0);
        Util.arrayFillNonAtomic(tmp, (short) 0, (short) tmp.length, (byte) 0);
        Util.arrayFillNonAtomic(nonce, (short) 0, NONCE_LEN, (byte) 0);
        for (short i = 0; i < PIN_MAX; i++) {
            if (newPin[i] != 0) {
                Util.arrayFillNonAtomic(newPin, (short) 0, PIN_MAX, (byte) 0);
                break;
            }
        }
    }

    /**
     * Whether this selection has the PIN: a right VERIFY_PIN (OwnerPIN's validated flag), or the
     * first SET_PIN, which counts as given for the selection that sent it.
     */
    private boolean pinGiven() {
        return pin.isValidated() || held[H_PIN_GIVEN] != 0;
    }

    /** Whether a slot holds value: a note, or change owed to it. */
    private boolean holdsValue() {
        for (short s = 0; s < SLOTS; s++) {
            if (state[s] == ST_NOTE || state[s] == ST_CHANGE) {
                return true;
            }
        }
        return false;
    }

    @Override
    public void process(APDU apdu) {
        byte[] buf = apdu.getBuffer();
        // The PIN's wait is the same VERIFY_PIN asked again and again: anything else, a SELECT
        // among it, begins the stretch again.
        if (selectingApplet() || buf[ISO7816.OFFSET_INS] != INS_VERIFY_PIN) {
            held[H_ASKED] = 0;
        }
        if (selectingApplet()) {
            buf[0] = VERSION_MAJOR;
            buf[1] = VERSION_MINOR;
            apdu.setOutgoingAndSend((short) 0, (short) 2);
            return;
        }
        if (buf[ISO7816.OFFSET_CLA] != CLA) {
            ISOException.throwIt(ISO7816.SW_CLA_NOT_SUPPORTED);
        }
        byte ins = buf[ISO7816.OFFSET_INS];
        switch (ins) {
            case INS_GEN_KEY:
                genKey(apdu, buf);
                break;
            case INS_GET_PUB:
                getPub(apdu, buf);
                break;
            case INS_SIGN:
                sign(apdu, buf);
                break;
            case INS_STATE_KEY:
                stateKey(apdu, buf);
                break;
            case INS_VERIFY_PIN:
                verifyPin(apdu, buf);
                break;
            case INS_SET_PIN:
                setPin(apdu, buf);
                break;
            case INS_CHANGE_PIN:
                changePin(apdu, buf);
                break;
            case INS_GET_NONCE:
                getNonce(apdu, buf);
                break;
            case INS_SET_NOTE:
                setNote(apdu, buf);
                break;
            case INS_GET_SLOT:
                getSlot(apdu, buf);
                break;
            case INS_GET_STATE:
                getState(apdu, buf);
                break;
            case INS_SET_CHANGE:
                setChange(apdu, buf);
                break;
            case INS_SET_LIMIT:
                setLimit(apdu, buf);
                break;
            case INS_SET_HEADER:
                setHeader(apdu, buf);
                break;
            case INS_ATTEST_INIT:
                attestInit(apdu, buf);
                break;
            case INS_SET_CERT:
                setCert(apdu, buf);
                break;
            case INS_GET_CERT:
                getCert(apdu, buf);
                break;
            case INS_ATTEST:
                attestSign(apdu, buf);
                break;
            case INS_SET_REGISTRY_KEY:
                setRegistryKey(apdu, buf);
                break;
            case INS_RELEASE:
                release(apdu, buf);
                break;
            default:
                ISOException.throwIt(ISO7816.SW_INS_NOT_SUPPORTED);
        }
    }

    /**
     * The gate on SIGN, checked before the slot and the message so a reader without the PIN
     * cannot probe which slots are keyed.
     */
    private void checkPinReady() {
        if (pinState != PIN_STATE_SET) {
            ISOException.throwIt(SW_PIN_NOT_SET);
        }
        if (!pinGiven()) {
            ISOException.throwIt(SW_PIN_NEEDED);
        }
    }

    /**
     * One PIN try, the data already received: {@code len} bytes at the data offset, bounded by
     * the caller. The try is counted, with the wait the next try would owe, before the compare
     * ({@link #countTry}); a right PIN then sets both to nothing in one atomic copy and returns with
     * this session validated. A wrong one answers {@code 63CX}, X the wrong tries in a row. Every
     * path wipes the APDU buffer and {@link #tmp}, so no PIN byte outlives the call. Only the
     * {@code len} bytes are compared: CHANGE_PIN's new PIN follows them and is not part of it.
     */
    private void tryPin(byte[] buf, short len) {
        // A try begins a fresh session: only a right PIN opens it again.
        held[H_PIN_GIVEN] = 0;
        try {
            countTry();
            if (pin.check(buf, ISO7816.OFFSET_CDATA, (byte) len)) {
                Util.arrayCopy(MARKS_CLEAR, (short) 0, pinMarks, PM_WRONG, (short) 3);
                return;
            }
        } finally {
            Util.arrayFillNonAtomic(buf, (short) 0, (short) buf.length, (byte) 0);
            Util.arrayFillNonAtomic(tmp, (short) 0, (short) tmp.length, (byte) 0);
        }
        ISOException.throwIt((short) (0x63C0 | pinMarks[PM_WRONG]));
    }

    /**
     * Before the PIN is compared: the try counted as wrong and the wait the next try would owe
     * after it (2^w askings, w the wrong tries in a row before it, at most PIN_OWED_MOST), both in
     * one transaction; then OwnerPIN's own counter is filled again, so it never runs out. Torn
     * anywhere, the try was counted with its wait or never made.
     */
    private void countTry() {
        short w = pinMarks[PM_WRONG];
        JCSystem.beginTransaction();
        pinMarks[PM_WRONG] = (byte) (w < PIN_WRONG_MOST ? w + 1 : PIN_WRONG_MOST);
        Util.setShort(pinMarks, PM_OWED, w < 14 ? (short) (1 << w) : PIN_OWED_MOST);
        JCSystem.commitTransaction();
        if (pin.getTriesRemaining() != PIN_TRIES) {
            pin.resetAndUnblock();
        }
    }

    /**
     * The wait before a try: the askings owed, served PIN_STRETCH at a time. Each asking of a
     * stretch is a signature's work answered {@code 6A98}; the asking after a stretch writes what
     * is still owed, less the stretch, in one transaction, and answers {@code 6A98} again while
     * anything is owed, or returns, and the caller goes on to the try. A card taken out of the
     * field loses no more than the stretch it was in. The count of a stretch is in RAM ({@link
     * #held}): an asking writes nothing persistent.
     */
    private void waitOwed(byte[] buf) {
        short owed = Util.getShort(pinMarks, PM_OWED);
        if (owed == 0) {
            return;
        }
        short stretch = owed < PIN_STRETCH ? owed : PIN_STRETCH;
        short asked = (short) (held[H_ASKED] + 1);
        if (asked <= stretch) {
            work(buf);
            // Counted after the work: an asking whose work ends early counts for nothing.
            held[H_ASKED] = (byte) asked;
            ISOException.throwIt(SW_KEEP_HOLDING);
        }
        held[H_ASKED] = 0;
        owed -= stretch;
        JCSystem.beginTransaction();
        Util.setShort(pinMarks, PM_OWED, owed);
        JCSystem.commitTransaction();
        if (owed != 0) {
            ISOException.throwIt(SW_KEEP_HOLDING);
        }
    }

    /**
     * One asking's work, thrown away: a BIP-340 signature with the work keys over random bytes,
     * where the signer keeps its nonce key in RAM; elsewhere its signature would write flash, so
     * SHA-256 rounds in RAM stand for it. A signature the signer refuses falls back to the rounds,
     * so a fault can make an asking neither free nor endless. Never with a slot key.
     */
    private void work(byte[] buf) {
        try {
            schnorr.random(tmp, (short) 0, (short) 32);
            if (workKeys != null && schnorr.keyInRam()) {
                try {
                    schnorr.sign((ECPrivateKey) workKeys.getPrivate(), (ECPublicKey) workKeys.getPublic(),
                        tmp, (short) 0, buf, (short) 0);
                    return;
                } catch (CardRuntimeException e) {
                    // the rounds below
                }
            }
            sha.reset();
            for (short i = 0; i < WORK_HASHES; i++) {
                sha.doFinal(tmp, (short) 0, (short) 32, tmp, (short) 0);
            }
        } finally {
            Util.arrayFillNonAtomic(buf, (short) 0, (short) 64, (byte) 0);
            Util.arrayFillNonAtomic(tmp, (short) 0, (short) tmp.length, (byte) 0);
        }
    }

    /**
     * Opens this session for SIGN, from a sealed PIN block (docs/OFFLINE-PROTOCOL.md § 9). Also starts a fresh session,
     * so a wrong PIN leaves nothing open. The frame is checked first, then the length, then the live nonce, all before
     * any receive and writing nothing; then the wait owed, before the data is read; then the opening, which spends the
     * nonce, and the try. An envelope that does not open is a wrong try ({@link #openingFailed}).
     */
    private void verifyPin(APDU apdu, byte[] buf) {
        short len = lcOf(buf);
        if (pinState != PIN_STATE_SET) {
            ISOException.throwIt(SW_PIN_NOT_SET);
        }
        // The frame before the length: a host that still sends the clear form (P1 = 00) is told 6B00.
        if (buf[ISO7816.OFFSET_P1] != SEALED || buf[ISO7816.OFFSET_P2] != 0) {
            ISOException.throwIt(ISO7816.SW_WRONG_P1P2);
        }
        if (len != SEAL_LC) {
            // Refused before any receive, so a wrong length costs neither a try nor a read.
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        if (held[H_NONCE] == 0) {
            // Before the wait: an envelope no nonce can open is served no wait.
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        try {
            waitOwed(buf);
            if (apdu.setIncomingAndReceive() < len) {
                ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
            }
            if (unseal(buf, len) < 0) {
                openingFailed(buf);
            }
            tryPin(buf, unblockPin(buf, ISO7816.OFFSET_CDATA));
        } finally {
            Util.arrayFillNonAtomic(buf, (short) 0, (short) buf.length, (byte) 0);
            forgetSeal();
        }
    }

    /**
     * The one-time PIN write, from a sealed PIN block, on a card with the registry key and no value, in a selection whose
     * SET_HEADER anchored the clock (an anchor made in an earlier selection does not count), under a live nonce. The
     * selection that sends it counts as having the PIN, so the host need not send VERIFY_PIN after it. An envelope that
     * does not open answers 6700, as a bad PIN block does, and writes nothing: no PIN is set, so there is no try to count.
     *
     * Before anything is written, the signer's check of the chip's engines (6F00 for a chip that
     * answers otherwise: such a card never takes a PIN, so never takes value). Just before the
     * commit point, the signer's one-time set-up and the work keys the PIN's wait signs with.
     */
    private void setPin(APDU apdu, byte[] buf) {
        short len = lcOf(buf);
        if (buf[ISO7816.OFFSET_P1] != SEALED || buf[ISO7816.OFFSET_P2] != 0) {
            ISOException.throwIt(ISO7816.SW_WRONG_P1P2);
        }
        if (pinState == PIN_STATE_SET) {
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        if (len != SEAL_LC) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        if (held[H_ANCHORED] == 0) {
            ISOException.throwIt(SW_NOT_ANCHORED);
        }
        if (!registryKeySet || holdsValue()) {
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        if (held[H_NONCE] == 0) {
            // Every refusal above leaves the nonce live.
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        try {
            // The signer's engine check: the chip's key agreement and point addition answer
            // known points, or 6F00 before the receive and before any write.
            schnorr.checkFraming(tmp, (short) 0);
            if (apdu.setIncomingAndReceive() < len) {
                ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
            }
            if (unseal(buf, len) < 0) {
                ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
            }
            short pl = unblockPin(buf, ISO7816.OFFSET_CDATA);
            // The same buffer as CHANGE_PIN's new half, which is only live inside a call.
            Util.arrayCopyNonAtomic(buf, ISO7816.OFFSET_CDATA, newPin, (short) 0, pl);
            // The signer's test of the chip's product, once a life: a card takes SET_PIN once
            // (a PIN set is never unset), and this is chipMul's one writer. Before the commit
            // point, so a SET_PIN torn after it runs it again and none is set without it.
            schnorr.chipMul = schnorr.chipMulInit();
            // The work keys, after the signer's RSA objects (so a chip short of room for them
            // fails here, loudly, not inside chipMulInit), and before the commit point: a SET_PIN
            // torn after this makes them again, and no PIN is set without them. Built once.
            if (workKeys == null) {
                workKeys = new KeyPair(KeyPair.ALG_EC_FP, KeyBuilder.LENGTH_EC_FP_256);
            }
            setParams((ECPrivateKey) workKeys.getPrivate(), (ECPublicKey) workKeys.getPublic());
            workKeys.genKeyPair();
            // The PIN write first, the state byte second: the state byte is the commit point, so
            // a tear before it leaves the card reading as not set and SET_PIN can run again.
            pin.update(newPin, (short) 0, (byte) pl);
            pinState = PIN_STATE_SET;
            held[H_PIN_GIVEN] = 1;
        } finally {
            Util.arrayFillNonAtomic(buf, (short) 0, (short) buf.length, (byte) 0);
            Util.arrayFillNonAtomic(newPin, (short) 0, (short) newPin.length, (byte) 0);
            forgetSeal();
        }
    }

    /**
     * A sealed old PIN block || new PIN block, each carrying its own length. Requires this session's PIN, and ends it.
     * An envelope that does not open is a wrong try, as a wrong old PIN is; a block whose length is not 4..8 is 6700
     * and no try.
     */
    private void changePin(APDU apdu, byte[] buf) {
        short lc = lcOf(buf);
        if (pinState != PIN_STATE_SET) {
            // And not-set outranks a bad frame, as in VERIFY_PIN: there is no old PIN to change.
            ISOException.throwIt(SW_PIN_NOT_SET);
        }
        if (buf[ISO7816.OFFSET_P1] != SEALED || buf[ISO7816.OFFSET_P2] != 0) {
            ISOException.throwIt(ISO7816.SW_WRONG_P1P2);
        }
        if (!pinGiven()) {
            ISOException.throwIt(SW_PIN_NEEDED);
        }
        if (lc != SEAL_LC_CHANGE) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        if (held[H_NONCE] == 0) {
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        try {
            // The whole data field is received here, before anything reads or wipes the buffer:
            // `setIncomingAndReceive` may be called only once per process() (APDU spec), and on
            // T=0 it "may return less than a full buffer of bytes". Reading only part of it would
            // open bytes the card has not received yet (stale buffer content).
            //
            // Not covered by a test: jCardSim is T=1 and always delivers the full field, and it
            // answers 6F00 to a command that ends with the APDU still partially received, so the
            // truncation cannot be injected from outside. The check below is the guard.
            if (apdu.setIncomingAndReceive() < lc) {
                ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
            }
            if (unseal(buf, lc) < 0) {
                openingFailed(buf);
            }
            short oldAt = ISO7816.OFFSET_CDATA;
            short newAt = (short) (oldAt + PIN_BLOCK);
            short old = (short) (buf[oldAt] & 0xFF);
            short young = (short) (buf[newAt] & 0xFF);
            if (old < PIN_MIN || old > PIN_MAX || young < PIN_MIN || young > PIN_MAX) {
                ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
            }
            // The new PIN is copied before the old PIN's check wipes the APDU buffer and tmp.
            // Into newPin, not tmp, because tryPin wipes tmp on its way out.
            Util.arrayCopyNonAtomic(buf, (short) (newAt + 1), newPin, (short) 0, young);
            unblockPin(buf, oldAt);
            // The old PIN is a try like VERIFY_PIN's, comparing exactly its `old` bytes: counted
            // before the compare, and a wrong one answers 63CX, ends the session and leaves the
            // next try its wait (none is served here: a session with the PIN has nothing owed).
            tryPin(buf, old);
            // A change ends the session, so the next CHANGE_PIN needs its own VERIFY_PIN.
            pin.reset();
            // OwnerPIN's update is atomic: a tear before it leaves the old PIN, after it the new
            // one, each with the marks the right old PIN cleared. The state byte is already set.
            pin.update(newPin, (short) 0, (byte) young);
        } finally {
            Util.arrayFillNonAtomic(buf, (short) 0, (short) buf.length, (byte) 0);
            Util.arrayFillNonAtomic(newPin, (short) 0, (short) newPin.length, (byte) 0);
            forgetSeal();
        }
    }

    /**
     * Lc from the header in the APDU buffer. Deliberately not {@code APDU.getIncomingLength()}:
     * jCardSim (and the JC 3.0.5 spec) throw {@code APDUException} if incoming data has not been
     * received yet, and every PIN path needs the length to validate the frame before receiving.
     * Only short APDUs of up to 127 bytes are taken; a longer or extended Lc reads as more than
     * two PINs, which every caller rejects.
     */
    private static short lcOf(byte[] buf) {
        if (buf[ISO7816.OFFSET_LC] < 0) {
            return (short) (2 * PIN_MAX + 1);
        }
        return (short) (buf[ISO7816.OFFSET_LC] & 0xFF);
    }

    /** HMAC message for STATE_KEY: ASCII "fedimintcard/state/v1". */
    private static final byte[] STATE_LABEL = {
        'f','e','d','i','m','i','n','t','c','a','r','d','/','s','t','a','t','e','/','v','1' };

    /**
     * STATE_KEY: answers HMAC-SHA256(secret, "fedimintcard/state/v1"), the same 32 bytes for the
     * life of the applet. Behind the PIN like SIGN (6A94 without this selection's VERIFY_PIN).
     * P1 and P2 must be 0. The secret is read into {@code tmp} and the HMAC worked in the APDU
     * buffer (bytes 32..127); both are wiped before the answer is sent from bytes 0..31.
     */
    private void stateKey(APDU apdu, byte[] buf) {
        checkPinReady();
        if (buf[ISO7816.OFFSET_P1] != 0 || buf[ISO7816.OFFSET_P2] != 0) {
            ISOException.throwIt(ISO7816.SW_WRONG_P1P2);
        }
        try {
            secret.getKey(tmp, (short) 0);
            schnorr.hmacSha256(tmp, (short) 0, STATE_LABEL, (short) 0, (short) STATE_LABEL.length,
                buf, (short) 32, buf, (short) 0);
        } finally {
            Util.arrayFillNonAtomic(tmp, (short) 0, (short) tmp.length, (byte) 0);
            Util.arrayFillNonAtomic(buf, (short) 32, (short) 96, (byte) 0);
        }
        apdu.setOutgoingAndSend((short) 0, (short) 32);
    }

    /** Checks P1/P2, returns the slot. P2 must be 0, or 1 when {@code allowOverwrite}. */
    private static short slot(byte[] buf, boolean allowOverwrite) {
        short p1 = buf[ISO7816.OFFSET_P1];
        byte p2 = buf[ISO7816.OFFSET_P2];
        if (p1 < 0 || p1 >= SLOTS || (p2 != 0 && !(allowOverwrite && p2 == 1))) {
            ISOException.throwIt(ISO7816.SW_WRONG_P1P2);
        }
        return p1;
    }

    private void sendPub(APDU apdu, byte[] buf, short s) {
        short n = pubs[s].getW(tmp, (short) 0);
        // 65 bytes 04||X||Y, or 33 bytes compressed: X is at offset 1 either way.
        boolean okEnc = (n == 65 && tmp[0] == 0x04)
            || (n == 33 && (tmp[0] == 0x02 || tmp[0] == 0x03));
        if (!okEnc) {
            ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
        }
        Util.arrayCopyNonAtomic(tmp, (short) 1, buf, (short) 0, (short) 32);
        apdu.setOutgoingAndSend((short) 0, (short) 32);
    }

    /**
     * Builds slot {@code s}'s key objects the first time it is keyed, once per slot for the life of
     * the applet: at most {@link #SLOTS} pairs, so this is bounded and never a per-tap leak. A card
     * out of persistent memory answers 6A84 and the slot stays empty.
     */
    private void keysFor(short s) {
        if (privs[s] != null) {
            return;
        }
        try {
            ECPublicKey pub = (ECPublicKey) KeyBuilder.buildKey(
                KeyBuilder.TYPE_EC_FP_PUBLIC, KeyBuilder.LENGTH_EC_FP_256, false);
            ECPrivateKey priv = (ECPrivateKey) KeyBuilder.buildKey(
                KeyBuilder.TYPE_EC_FP_PRIVATE, KeyBuilder.LENGTH_EC_FP_256, false);
            pubs[s] = pub;
            privs[s] = priv;
        } catch (SystemException e) {
            ISOException.throwIt(ISO7816.SW_FILE_FULL);
        }
    }

    private void genKey(APDU apdu, byte[] buf) {
        short s = slot(buf, true);
        keysFor(s);
        boolean overwrite = buf[ISO7816.OFFSET_P2] == 1;
        if (apdu.setIncomingAndReceive() != 0) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        // Slot 0's key is the one the certificate names, and a certificate is taken only once, before
        // the first SET_PIN. Re-keying slot 0 after SET_CERT would leave a card no host trusts and no
        // registry can certify again, so the re-key form refuses it, before anything is written.
        if (overwrite && s == 0 && cert[0] != 0) {
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        byte st = state[s];
        if (st != ST_EMPTY) {
            if (!overwrite) {
                ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
            }
            // A note's key, or a key owed change, holds value: replacing it would destroy the
            // value, so 3.0 never does. A KEYED slot has none yet, a SPENT one none left.
            if (st == ST_NOTE || st == ST_CHANGE) {
                ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
            }
            // Replacing a key destroys whatever notes were issued to it, so once the card has a PIN it
            // takes the same PIN as spending them: without this, any reader in range could wipe a slot
            // with one APDU. Before the first SET_PIN nothing can be spent, so provisioning may still
            // re-key freely.
            if (pinState == PIN_STATE_SET) {
                checkPinReady();
            }
            // The state byte is the commit point, written alone (a single-byte write is atomic):
            // from here a tear leaves an empty slot, never an old key the card might sign with
            // again, nor a spent hash that a new key would inherit.
            state[s] = ST_EMPTY;
            Util.arrayFillNonAtomic(spentHash, (short) (s * 32), (short) 32, (byte) 0);
            Util.arrayFillNonAtomic(spentSig, (short) (s * 64), (short) 64, (byte) 0);
            Util.arrayFillNonAtomic(notes, (short) (s * NOTE_LEN), NOTE_LEN, (byte) 0);
            Util.arrayFillNonAtomic(openings, (short) (s * OPEN_LEN), OPEN_LEN, (byte) 0);
            privs[s].clearKey();
            setParams(privs[s], null);
        }
        ECPrivateKey gp = (ECPrivateKey) gen.getPrivate();
        boolean ok = false;
        try {
            gen.genKeyPair();
            short len = gp.getS(tmp, (short) 0);
            // getS may strip leading zeros or add one on silicon: normalise to 32 bytes.
            SchnorrHW.alignScalar(tmp, (short) 0, len);
            // Re-assert domain parameters first: a tear after clearKey must not leave a parameterless key.
            setParams(privs[s], pubs[s]);
            privs[s].setS(tmp, (short) 0, (short) 32);
            // Read back: the signer refuses a scalar of more than 32 bytes, so a key the chip
            // gives back longer is refused here and the slot stays EMPTY, never a note's key that
            // SIGN cannot use.
            short sl = privs[s].getS(tmp, (short) 0);
            if (sl < 1 || sl > 32) {
                ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
            }
            short wl = ((ECPublicKey) gen.getPublic()).getW(tmp, (short) 0);
            pubs[s].setW(tmp, (short) 0, wl);
            ok = true;
        } finally {
            // The generated scalar must not outlive this call, even on a throw.
            gp.clearKey();
            Util.arrayFillNonAtomic(tmp, (short) 0, (short) tmp.length, (byte) 0);
            // clearKey may drop the domain parameters; restore them for the next genKeyPair.
            setParams(gp, null);
            if (!ok) {
                privs[s].clearKey();
                setParams(privs[s], null);
            }
        }
        state[s] = ST_KEYED;
        sendPub(apdu, buf, s);
    }

    private void getPub(APDU apdu, byte[] buf) {
        short s = slot(buf, false);
        if (apdu.setIncomingAndReceive() != 0) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        if (state[s] == ST_EMPTY) {
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        sendPub(apdu, buf, s);
    }

    /**
     * SIGN, at most one transaction per slot (3.0). The signature is made in RAM and checked
     * there by the signer (BIP-340's verify, before a byte of it is written): one that fails the
     * check answers 6F00 and the slot stays a note, with nothing stored. Only after the signer
     * returns are the slot's spent hash, its signature, its state and the day's count committed
     * together, and only then does the signature leave the card. A tear before the commit has
     * spent nothing and given nothing; a tear after it leaves a spent slot whose one transaction
     * can be answered again from storage.
     */
    private void sign(APDU apdu, byte[] buf) {
        // The gate comes first: without the PIN, a reader learns nothing about the slots.
        checkPinReady();
        short s = slot(buf, false);
        short got = apdu.setIncomingAndReceive();
        if (got != 32 || apdu.getIncomingLength() != 32) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        byte st = state[s];
        if (st == ST_SPENT) {
            // The one transaction this slot signed, asked again (an answer lost on the way): it
            // spends nothing new. Anything else would be the same note spent twice.
            if (Util.arrayCompare(buf, ISO7816.OFFSET_CDATA, spentHash, (short) (s * 32), (short) 32) != 0) {
                ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
            }
            // Answered from storage, as cashucard's SPEND_ALL_AGAIN: the key computes one
            // signature per note it holds (a RELEASE makes the slot a note again, and its next
            // SIGN signs anew), which leaves a power analysis little to compare.
            Util.arrayCopyNonAtomic(spentSig, (short) (s * 64), buf, (short) 0, (short) 64);
            apdu.setOutgoingAndSend((short) 0, (short) 64);
            return;
        }
        if (st != ST_NOTE) {
            // No note to spend: an empty or keyed slot, or one still owed its change.
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        Util.arrayCopyNonAtomic(buf, ISO7816.OFFSET_CDATA, x, X_HASH, (short) 32);
        dayAfter((short) (s * NOTE_LEN));
        // The message is the copy in x, so message and output never overlap. sign() throws 6F00
        // for a signature that fails its check, before anything here is written.
        schnorr.sign(privs[s], pubs[s], x, X_HASH, buf, (short) 0);
        JCSystem.beginTransaction();
        Util.arrayCopy(x, X_HASH, spentHash, (short) (s * 32), (short) 32);
        Util.arrayCopy(buf, (short) 0, spentSig, (short) (s * 64), (short) 64);
        state[s] = ST_SPENT;
        Util.arrayCopy(x, X_WINDOW, clock, CK_WINDOW, (short) 4);
        Util.arrayCopy(x, X_SPENT, clock, CK_SPENT, (short) 8);
        JCSystem.commitTransaction();
        apdu.setOutgoingAndSend((short) 0, (short) 64);
    }

    /**
     * The day's window and count as they would be after spending the note at notes[n]: into x
     * (X_WINDOW, X_SPENT), refused with 6A95 past a limit. A day is 86 400 seconds of the card's
     * clock from the window's start; with no header ever shown, the window never starts and the
     * limit holds for good, which offline is the point.
     */
    private void dayAfter(short n) {
        Util.arrayCopyNonAtomic(clock, CK_WINDOW, x, X_WINDOW, (short) 4);
        Util.arrayCopyNonAtomic(clock, CK_SPENT, x, X_SPENT, (short) 8);
        if (!isZero(clock, CK_NOW, (short) 4)) {
            if (isZero(x, X_WINDOW, (short) 4)) {
                Util.arrayCopyNonAtomic(clock, CK_NOW, x, X_WINDOW, (short) 4);
            } else if (windowIsOver()) {
                Util.arrayCopyNonAtomic(clock, CK_NOW, x, X_WINDOW, (short) 4);
                Util.arrayFillNonAtomic(x, X_SPENT, (short) 8, (byte) 0);
            }
        }
        boolean wrapped = add64(x, X_SPENT, notes, n);
        if (!isZero(clock, CK_LIMIT, (short) 8)
            && (wrapped || cmp(x, X_SPENT, clock, CK_LIMIT, (short) 8) > 0)) {
            ISOException.throwIt(SW_OVER_LIMIT);
        }
    }

    /** Whether now is at least a day past the window's start. A start too late to add a day to never ends. */
    private boolean windowIsOver() {
        Util.arrayCopyNonAtomic(clock, CK_WINDOW, x, X_NUM, (short) 4);
        short carry = 0;
        for (short i = 3; i >= 0; i--) {
            short v = (short) ((x[(short) (X_NUM + i)] & 0xFF) + (DAY_SECONDS[i] & 0xFF) + carry);
            x[(short) (X_NUM + i)] = (byte) v;
            carry = (short) ((v >> 8) & 1);
        }
        if (carry != 0) return false;
        return cmp(clock, CK_NOW, x, X_NUM, (short) 4) >= 0;
    }

    /** a += b, eight bytes each, big-endian; answers whether it wrapped. */
    private static boolean add64(byte[] a, short aOff, byte[] b, short bOff) {
        short carry = 0;
        for (short i = 7; i >= 0; i--) {
            short v = (short) ((a[(short) (aOff + i)] & 0xFF) + (b[(short) (bOff + i)] & 0xFF) + carry);
            a[(short) (aOff + i)] = (byte) v;
            carry = (short) ((v >> 8) & 1);
        }
        return carry != 0;
    }

    /** Unsigned big-endian compare of len bytes: negative, 0 or positive. Public values only. */
    private static short cmp(byte[] a, short aOff, byte[] b, short bOff, short len) {
        for (short i = 0; i < len; i++) {
            short d = (short) ((a[(short) (aOff + i)] & 0xFF) - (b[(short) (bOff + i)] & 0xFF));
            if (d != 0) return d;
        }
        return 0;
    }

    private static boolean isZero(byte[] a, short off, short len) {
        for (short i = 0; i < len; i++) {
            if (a[(short) (off + i)] != 0) return false;
        }
        return true;
    }

    /** Receives exactly {@code len} bytes of data, or 6700. */
    private static void receiveExactly(APDU apdu, short len) {
        if (receiveAll(apdu) != len) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
    }

    /**
     * The whole data field, however the transport delivers it: over contactless the J3R180 gave
     * a 130-byte SET_CERT in pieces, and one setIncomingAndReceive took only the first (F0).
     * Answers the length received, which is the Lc when all of it came.
     */
    private static short receiveAll(APDU apdu) {
        // setIncomingAndReceive first: getIncomingLength is not defined before it (JC 3.0.5, jCardSim)
        short got = apdu.setIncomingAndReceive();
        short want = apdu.getIncomingLength();
        short off = (short) (ISO7816.OFFSET_CDATA + got);
        while (got < want) {
            short n = apdu.receiveBytes(off);
            if (n <= 0) {
                break;
            }
            got += n;
            off += n;
        }
        return got;
    }

    /**
     * SET_NOTE: the note on a KEYED slot, or the finished change on a CHANGE slot (whose amount
     * must match), with the registry's signature over the slot's own key and the note, checked
     * under the registry key as RELEASE's is. No PIN: loading spends nothing, as for a holder's
     * load. The check writes nothing; the note's data and its state are one commit, and a CHANGE
     * slot's opening is let go after it.
     */
    private void setNote(APDU apdu, byte[] buf) {
        short s = slot(buf, false);
        short len = receiveAll(apdu);
        short sigLen = (short) (len - NOTE_LEN);
        if (len != apdu.getIncomingLength() || sigLen < 8 || sigLen > 72) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        byte st = state[s];
        short n = (short) (s * NOTE_LEN);
        if (st == ST_CHANGE) {
            if (Util.arrayCompare(buf, ISO7816.OFFSET_CDATA, notes, n, (short) 8) != 0) {
                ISOException.throwIt(ISO7816.SW_WRONG_DATA);
            }
        } else if (st != ST_KEYED) {
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        if (!registryKeySet) {
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        if (isZero(buf, ISO7816.OFFSET_CDATA, (short) 8)) {
            ISOException.throwIt(ISO7816.SW_WRONG_DATA);
        }
        // label (18) || the slot's x (32) || the note (56), built from byte 134 of the buffer: past
        // the longest data (5 + 56 + 72 = 133), so the signature it is checked against stays whole
        short m = NOTE_MSG;
        Util.arrayCopyNonAtomic(NOTE_LABEL, (short) 0, buf, m, (short) 18);
        short k = pubs[s].getW(tmp, (short) 0);
        if (!((k == 65 && tmp[0] == 0x04) || (k == 33 && (tmp[0] == 0x02 || tmp[0] == 0x03)))) {
            ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
        }
        Util.arrayCopyNonAtomic(tmp, (short) 1, buf, (short) (m + 18), (short) 32);
        Util.arrayFillNonAtomic(tmp, (short) 0, (short) tmp.length, (byte) 0);
        Util.arrayCopyNonAtomic(buf, ISO7816.OFFSET_CDATA, buf, (short) (m + 50), NOTE_LEN);
        ecdsa.init(registryKey, Signature.MODE_VERIFY);
        boolean ok;
        try {
            ok = ecdsa.verify(buf, m, (short) 106, buf, (short) (ISO7816.OFFSET_CDATA + NOTE_LEN), sigLen);
        } catch (javacard.security.CryptoException e) {
            ok = false;
        }
        if (!ok) {
            ISOException.throwIt(ISO7816.SW_SECURITY_STATUS_NOT_SATISFIED);
        }
        JCSystem.beginTransaction();
        Util.arrayCopy(buf, ISO7816.OFFSET_CDATA, notes, n, NOTE_LEN);
        state[s] = ST_NOTE;
        JCSystem.commitTransaction();
        Util.arrayFillNonAtomic(openings, (short) (s * OPEN_LEN), OPEN_LEN, (byte) 0);
    }

    /**
     * SET_CHANGE: a KEYED slot is owed change from an offline payment. The card keeps what an
     * online till needs to finish the note later: the amount, the transaction and output, and the
     * blinding key (the till blinds; the card cannot hash to BLS12-381). The key never leaves
     * the card, so only this card can spend the change, as in cashucard's GET_CHANGE. After
     * SET_PIN it needs this selection's PIN (its one sender, the offline POS, holds a PIN session).
     */
    private void setChange(APDU apdu, byte[] buf) {
        short s = slot(buf, false);
        if (pinState == PIN_STATE_SET && !pinGiven()) {
            ISOException.throwIt(SW_PIN_NEEDED);
        }
        receiveExactly(apdu, (short) (8 + OPEN_LEN));
        if (state[s] != ST_KEYED) {
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        if (isZero(buf, ISO7816.OFFSET_CDATA, (short) 8)) {
            ISOException.throwIt(ISO7816.SW_WRONG_DATA);
        }
        short n = (short) (s * NOTE_LEN);
        JCSystem.beginTransaction();
        Util.arrayCopy(buf, ISO7816.OFFSET_CDATA, notes, n, (short) 8);
        Util.arrayCopy(buf, (short) (ISO7816.OFFSET_CDATA + 8), openings, (short) (s * OPEN_LEN), OPEN_LEN);
        state[s] = ST_CHANGE;
        JCSystem.commitTransaction();
        Util.arrayFillNonAtomic(notes, (short) (n + 8), (short) 48, (byte) 0);
    }

    /** GET_SLOT: state, x-only key (zeros when empty), note, spent hash, opening. Public: no PIN. */
    private void getSlot(APDU apdu, byte[] buf) {
        short s = slot(buf, false);
        if (apdu.setIncomingAndReceive() != 0) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        buf[0] = state[s];
        if (state[s] == ST_EMPTY) {
            Util.arrayFillNonAtomic(buf, (short) 1, (short) 32, (byte) 0);
        } else {
            short n = pubs[s].getW(tmp, (short) 0);
            if (!((n == 65 && tmp[0] == 0x04) || (n == 33 && (tmp[0] == 0x02 || tmp[0] == 0x03)))) {
                ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
            }
            Util.arrayCopyNonAtomic(tmp, (short) 1, buf, (short) 1, (short) 32);
            Util.arrayFillNonAtomic(tmp, (short) 0, (short) tmp.length, (byte) 0);
        }
        Util.arrayCopyNonAtomic(notes, (short) (s * NOTE_LEN), buf, (short) 33, NOTE_LEN);
        Util.arrayCopyNonAtomic(spentHash, (short) (s * 32), buf, (short) 89, (short) 32);
        Util.arrayCopyNonAtomic(openings, (short) (s * OPEN_LEN), buf, (short) 121, OPEN_LEN);
        apdu.setOutgoingAndSend((short) 0, (short) 187);
    }

    /** GET_STATE: every slot's state, the clock and the limit, and what attestation has. */
    private void getState(APDU apdu, byte[] buf) {
        if (apdu.setIncomingAndReceive() != 0) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        Util.arrayCopyNonAtomic(state, (short) 0, buf, (short) 0, SLOTS);
        Util.arrayCopyNonAtomic(clock, CK_NOW, buf, (short) 32, (short) 4);
        Util.arrayCopyNonAtomic(clock, CK_WINDOW, buf, (short) 36, (short) 4);
        Util.arrayCopyNonAtomic(clock, CK_LIMIT, buf, (short) 40, (short) 8);
        Util.arrayCopyNonAtomic(clock, CK_SPENT, buf, (short) 48, (short) 8);
        buf[56] = cert[0];
        buf[57] = attestMade ? (byte) 1 : (byte) 0;
        buf[58] = registryKeySet ? (byte) 1 : (byte) 0;
        // The PIN: its state, the wrong tries in a row and the askings owed before the next try,
        // so a till can say how long to hold before it sends the PIN.
        buf[59] = pinState;
        Util.arrayCopyNonAtomic(pinMarks, PM_WRONG, buf, (short) 60, (short) 3);
        // Whether this selection anchored the clock, and the signer: bit 0 the chip's RSA path
        // for the product, bit 1 a nonce key in RAM (the PIN's wait then writes nothing).
        buf[63] = held[H_ANCHORED];
        buf[64] = (byte) ((schnorr.chipMul ? 1 : 0) | (schnorr.keyInRam() ? 2 : 0));
        apdu.setOutgoingAndSend((short) 0, STATE_LEN);
    }

    /** SET_LIMIT: the day's limit in msat, set at provisioning, before the PIN; 0 for none. */
    private void setLimit(APDU apdu, byte[] buf) {
        receiveExactly(apdu, (short) 8);
        if (pinState != PIN_STATE_NOT_SET) {
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        Util.arrayCopy(buf, ISO7816.OFFSET_CDATA, clock, CK_LIMIT, (short) 8);
    }

    /**
     * SET_HEADER, after cashucard's: a Bitcoin block header moves the card's clock to its time when
     * that is later, if the header has its own bits' work, at least the floor's, at least a quarter
     * of the hardest the card has taken, and is not weak. Weak: its target is over twice the
     * hardest's, or, before any hardest, its work is under twice the floor's. A header that fails
     * any of these answers 6A93 and writes nothing. A header taken raises the hardest only where it
     * is harder. A real header exists only once the world has mined it, so no terminal can make the
     * day turn early. Anyone may send one. Answers now (4).
     *
     * While no PIN is set and no slot holds value, a header taken is an anchor: in one transaction
     * the clock goes to its time, even an earlier one, the hardest is raised where it is harder
     * (never lowered), and the day's window and its spent count are cleared. This selection then
     * counts as anchored, which SET_PIN asks for.
     */
    private void setHeader(APDU apdu, byte[] buf) {
        receiveExactly(apdu, HEADER_LEN);
        short at = ISO7816.OFFSET_CDATA;
        sha.reset();
        sha.doFinal(buf, at, HEADER_LEN, x, X_OUT);
        sha.reset();
        sha.doFinal(x, X_OUT, (short) 32, x, X_OUT);
        // Bitcoin reads the hash little-endian: turn it round into a big-endian number at X_OUT+32
        for (short i = 0; i < 32; i++) {
            x[(short) (X_OUT + 32 + i)] = x[(short) (X_OUT + 31 - i)];
        }
        Util.arrayCopyNonAtomic(x, (short) (X_OUT + 32), x, X_OUT, (short) 32);
        targetOf(buf, (short) (at + 72), x, (short) (X_OUT + 32));
        if (cmp(x, X_OUT, x, (short) (X_OUT + 32), (short) 32) > 0) {
            ISOException.throwIt(SW_LITTLE_WORK);
        }
        targetOf(FLOOR_BITS, (short) 0, x, X_FLOOR);
        if (cmp(x, (short) (X_OUT + 32), x, X_FLOOR, (short) 32) > 0) {
            ISOException.throwIt(SW_LITTLE_WORK);
        }
        boolean seen = !isZero(clock, CK_BITS, (short) 4);
        if (seen) {
            targetOf(clock, CK_BITS, x, X_FLOOR);
            timesFour(x, X_FLOOR);
            if (cmp(x, (short) (X_OUT + 32), x, X_FLOOR, (short) 32) > 0) {
                ISOException.throwIt(SW_LITTLE_WORK);
            }
        }
        // whether it is harder than the hardest taken, and whether it is weak (R's checkHeader)
        boolean harder = true;
        if (seen) {
            targetOf(clock, CK_BITS, x, X_FLOOR);
            harder = cmp(x, (short) (X_OUT + 32), x, X_FLOOR, (short) 32) < 0;
            // the weak bar: twice the hardest's target
            twice(x, X_FLOOR);
        } else {
            // before any hardest: the header's target doubled (nothing reads it after) against the
            // floor's, which X_FLOOR still holds, so work under twice the floor's is weak
            twice(x, (short) (X_OUT + 32));
        }
        if (cmp(x, (short) (X_OUT + 32), x, X_FLOOR, (short) 32) > 0) {
            ISOException.throwIt(SW_LITTLE_WORK);
        }
        // the block's time, big-endian
        x[X_NUM] = buf[(short) (at + 71)];
        x[(short) (X_NUM + 1)] = buf[(short) (at + 70)];
        x[(short) (X_NUM + 2)] = buf[(short) (at + 69)];
        x[(short) (X_NUM + 3)] = buf[(short) (at + 68)];
        if (pinState != PIN_STATE_SET && !holdsValue()) {
            // the anchor: the new record built in scratch, then written in one transaction
            Util.arrayCopyNonAtomic(clock, (short) 0, x, X_REC, CK_LEN);
            Util.arrayCopyNonAtomic(x, X_NUM, x, (short) (X_REC + CK_NOW), (short) 4);
            Util.arrayFillNonAtomic(x, (short) (X_REC + CK_WINDOW), (short) 4, (byte) 0);
            if (harder) {
                Util.arrayCopyNonAtomic(buf, (short) (at + 72), x, (short) (X_REC + CK_BITS), (short) 4);
            }
            Util.arrayFillNonAtomic(x, (short) (X_REC + CK_SPENT), (short) 8, (byte) 0);
            JCSystem.beginTransaction();
            Util.arrayCopy(x, X_REC, clock, (short) 0, CK_LEN);
            JCSystem.commitTransaction();
            held[H_ANCHORED] = 1;
        } else if (cmp(x, X_NUM, clock, CK_NOW, (short) 4) > 0) {
            JCSystem.beginTransaction();
            Util.arrayCopy(x, X_NUM, clock, CK_NOW, (short) 4);
            if (harder) {
                Util.arrayCopy(buf, (short) (at + 72), clock, CK_BITS, (short) 4);
            }
            JCSystem.commitTransaction();
        }
        Util.arrayCopyNonAtomic(clock, CK_NOW, buf, (short) 0, (short) 4);
        apdu.setOutgoingAndSend((short) 0, (short) 4);
    }

    /**
     * The target a header's `bits` name, as a 32-byte big-endian number: the three-byte mantissa
     * placed `exponent` bytes up from the bottom. A size no header could carry, or a negative
     * mantissa, is 6A80. From cashucard's applet.
     */
    private static void targetOf(byte[] src, short off, byte[] out, short outOff) {
        short exponent = (short) (src[(short) (off + 3)] & 0xFF);
        if (exponent < 1 || exponent > 32 || (src[(short) (off + 2)] & 0x80) != 0) {
            ISOException.throwIt(ISO7816.SW_WRONG_DATA);
        }
        Util.arrayFillNonAtomic(out, outOff, (short) 32, (byte) 0);
        for (short k = 0; k < 3; k++) {
            short idx = (short) (32 - exponent + k);
            if (idx < 32) {
                out[(short) (outOff + idx)] = src[(short) (off + 2 - k)];
            }
        }
    }

    /** Twice a 32-byte big-endian number, in place; too large to hold becomes the largest there is. */
    private static void twice(byte[] a, short off) {
        short carry = 0;
        for (short i = 31; i >= 0; i--) {
            short v = (short) (((a[(short) (off + i)] & 0xFF) << 1) | carry);
            a[(short) (off + i)] = (byte) v;
            carry = (short) ((v >> 8) & 0x01);
        }
        if (carry != 0) {
            Util.arrayFillNonAtomic(a, off, (short) 32, (byte) 0xFF);
        }
    }

    /** Four times a 32-byte big-endian number, in place; too large to hold becomes the largest there is. */
    private static void timesFour(byte[] a, short off) {
        short carry = 0;
        for (short i = 31; i >= 0; i--) {
            short v = (short) (((a[(short) (off + i)] & 0xFF) << 2) | carry);
            a[(short) (off + i)] = (byte) v;
            carry = (short) ((v >> 8) & 0x03);
        }
        if (carry != 0) {
            Util.arrayFillNonAtomic(a, off, (short) 32, (byte) 0xFF);
        }
    }

    private static void p256(javacard.security.ECKey k) {
        k.setFieldFP(R1_P, (short) 0, (short) 32);
        k.setA(R1_A, (short) 0, (short) 32);
        k.setB(R1_B, (short) 0, (short) 32);
        k.setG(R1_G, (short) 0, (short) 65);
        k.setR(R1_N, (short) 0, (short) 32);
        k.setK((short) 1);
    }

    /** Provisioning only: no PIN yet, no certificate yet. */
    private void beforePinAndCert() {
        if (pinState != PIN_STATE_NOT_SET || cert[0] != 0) {
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
    }

    /** ATTEST_INIT: (re)makes the P-256 attestation key and answers its public point. */
    private void attestInit(APDU apdu, byte[] buf) {
        if (apdu.setIncomingAndReceive() != 0) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        beforePinAndCert();
        // The PIN key and the field keys of its curve check: built at the first ATTEST_INIT, not at install, for the
        // reason in the constructor (the J3R180's bounded install). Each is built once. Then the PIN key is made and its
        // binding cleared, and only then the attestation key, so a torn ATTEST_INIT never leaves attestMade set over a
        // PIN key built but not made.
        if (pinKey == null) {
            pinKey = new KeyPair(KeyPair.ALG_EC_FP, KeyBuilder.LENGTH_EC_FP_256);
        }
        setParams((ECPrivateKey) pinKey.getPrivate(), (ECPublicKey) pinKey.getPublic());
        if (rsa == null) {
            rsa = Cipher.getInstance(Cipher.ALG_RSA_NOPAD, false);
        }
        if (rsaCube == null) {
            rsaCube = (RSAPrivateKey) KeyBuilder.buildKey(KeyBuilder.TYPE_RSA_PRIVATE, KeyBuilder.LENGTH_RSA_512, false);
        }
        if (rsaRoot == null) {
            rsaRoot = (RSAPrivateKey) KeyBuilder.buildKey(KeyBuilder.TYPE_RSA_PRIVATE, KeyBuilder.LENGTH_RSA_512, false);
        }
        if (rsaEuler == null) {
            rsaEuler = (RSAPrivateKey) KeyBuilder.buildKey(KeyBuilder.TYPE_RSA_PRIVATE, KeyBuilder.LENGTH_RSA_512, false);
        }
        // set at every ATTEST_INIT, so a key built by one torn before its modulus is set is set by the next
        fieldKey(rsaCube, RSA_D_CUBE);
        fieldKey(rsaRoot, RSA_D_ROOT);
        fieldKey(rsaEuler, RSA_D_EULER);
        pinKey.genKeyPair();
        pinKeySig[0] = 0;
        attest.genKeyPair();
        attestMade = true;
        short n = ((ECPublicKey) attest.getPublic()).getW(buf, (short) 0);
        apdu.setOutgoingAndSend((short) 0, n);
    }

    /** SET_CERT: the registry's certificate over this card's keys, once, before the PIN. */
    private void setCert(APDU apdu, byte[] buf) {
        short len = receiveAll(apdu);
        if (len < 1 || len > CERT_MAX || apdu.getIncomingLength() != len) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        beforePinAndCert();
        if (!attestMade) {
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        JCSystem.beginTransaction();
        Util.arrayCopy(buf, ISO7816.OFFSET_CDATA, cert, (short) 1, len);
        cert[0] = (byte) len;
        JCSystem.commitTransaction();
    }

    private void getCert(APDU apdu, byte[] buf) {
        if (apdu.setIncomingAndReceive() != 0) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        short len = (short) (cert[0] & 0xFF);
        if (len == 0) {
            ISOException.throwIt(ISO7816.SW_RECORD_NOT_FOUND);
        }
        Util.arrayCopyNonAtomic(cert, (short) 1, buf, (short) 0, len);
        apdu.setOutgoingAndSend((short) 0, len);
    }

    /**
     * ATTEST: this is the card the registry certified. ECDSA over the label, the till's challenge
     * and slot 0's key (the card's identity), so an answer cannot be replayed for another
     * challenge or carried to another card. No PIN: a till checks a card before asking for one.
     */
    private void attestSign(APDU apdu, byte[] buf) {
        receiveExactly(apdu, (short) 32);
        if (!attestMade || cert[0] == 0 || state[0] == ST_EMPTY) {
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        // label (16) || challenge (32) || slot 0's x (32), assembled in the buffer from byte 128
        short m = (short) 128;
        Util.arrayCopyNonAtomic(ATTEST_LABEL, (short) 0, buf, m, (short) 16);
        Util.arrayCopyNonAtomic(buf, ISO7816.OFFSET_CDATA, buf, (short) (m + 16), (short) 32);
        short n = pubs[0].getW(tmp, (short) 0);
        if (!((n == 65 && tmp[0] == 0x04) || (n == 33 && (tmp[0] == 0x02 || tmp[0] == 0x03)))) {
            ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
        }
        Util.arrayCopyNonAtomic(tmp, (short) 1, buf, (short) (m + 48), (short) 32);
        ecdsa.init(attest.getPrivate(), Signature.MODE_SIGN);
        short sl = ecdsa.sign(buf, m, (short) 80, buf, (short) 0);
        apdu.setOutgoingAndSend((short) 0, sl);
    }

    /** SET_REGISTRY_KEY: the registry's P-256 certificate key, at provisioning, before the PIN. */
    private void setRegistryKey(APDU apdu, byte[] buf) {
        receiveExactly(apdu, (short) 65);
        if (pinState != PIN_STATE_NOT_SET) {
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        if (buf[ISO7816.OFFSET_CDATA] != 0x04) {
            ISOException.throwIt(ISO7816.SW_WRONG_DATA);
        }
        try {
            registryKey.setW(buf, ISO7816.OFFSET_CDATA, (short) 65);
        } catch (javacard.security.CryptoException e) {
            ISOException.throwIt(ISO7816.SW_WRONG_DATA);
        }
        registryKeySet = true;
    }

    /**
     * RELEASE: the registry says a transaction will never be accepted (the federation refused it,
     * or the registry refused its report as a double spend), so the card may undo what it did for
     * it. A SPENT slot whose one transaction is txid goes back to NOTE, so its note can be spent
     * again: it never was. A CHANGE slot owed by txid goes back to KEYED: that change will never
     * be issued. Nothing else changes, and a release names one card (slot 0's key), one slot and
     * one transaction, so it cannot be replayed onto a later spend of the same slot. No PIN: it
     * moves no value. The day's count is left as it was.
     */
    private void release(APDU apdu, byte[] buf) {
        short s = slot(buf, false);
        short len = receiveAll(apdu);
        short sigLen = (short) (len - 32);
        if (len != apdu.getIncomingLength() || sigLen < 8 || sigLen > 72) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        if (!registryKeySet || state[0] == ST_EMPTY) {
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        short txid = ISO7816.OFFSET_CDATA;
        // label (17) || slot 0's x (32) || slot (1) || txid (32), built from byte 128 of the buffer
        short m = (short) 128;
        Util.arrayCopyNonAtomic(RELEASE_LABEL, (short) 0, buf, m, (short) 17);
        short n = pubs[0].getW(tmp, (short) 0);
        if (!((n == 65 && tmp[0] == 0x04) || (n == 33 && (tmp[0] == 0x02 || tmp[0] == 0x03)))) {
            ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
        }
        Util.arrayCopyNonAtomic(tmp, (short) 1, buf, (short) (m + 17), (short) 32);
        buf[(short) (m + 49)] = (byte) s;
        Util.arrayCopyNonAtomic(buf, txid, buf, (short) (m + 50), (short) 32);
        ecdsa.init(registryKey, Signature.MODE_VERIFY);
        boolean ok;
        try {
            ok = ecdsa.verify(buf, m, (short) 82, buf, (short) (txid + 32), sigLen);
        } catch (javacard.security.CryptoException e) {
            ok = false;
        }
        if (!ok) {
            ISOException.throwIt(ISO7816.SW_SECURITY_STATUS_NOT_SATISFIED);
        }
        byte st = state[s];
        if (st == ST_SPENT
            && Util.arrayCompare(buf, txid, spentHash, (short) (s * 32), (short) 32) == 0) {
            JCSystem.beginTransaction();
            state[s] = ST_NOTE;
            Util.arrayFillNonAtomic(spentHash, (short) (s * 32), (short) 32, (byte) 0);
            JCSystem.commitTransaction();
            Util.arrayFillNonAtomic(spentSig, (short) (s * 64), (short) 64, (byte) 0);
        } else if (st == ST_CHANGE
            && Util.arrayCompare(buf, txid, openings, (short) (s * OPEN_LEN), (short) 32) == 0) {
            JCSystem.beginTransaction();
            state[s] = ST_KEYED;
            JCSystem.commitTransaction();
            Util.arrayFillNonAtomic(openings, (short) (s * OPEN_LEN), OPEN_LEN, (byte) 0);
            Util.arrayFillNonAtomic(notes, (short) (s * NOTE_LEN), NOTE_LEN, (byte) 0);
        } else {
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        buf[0] = state[s];
        apdu.setOutgoingAndSend((short) 0, (short) 1);
    }

    // ---------------------------------------------------------------------------------------------
    // The sealed PIN (docs/OFFLINE-PROTOCOL.md § 9). Ported from Cashu Card's applet (MIT, see NOTICE):
    // processGetPinKey, unseal, sealBlock, unblockPin, and the curve check requireFullPoint with its
    // field code on the chip's RSA. Ours differs in the labels, in the binding (ECDSA P-256 by the
    // attestation key, not BIP-340 by a payment key), in where the working room is, and in taking
    // the sealed form only.
    // ---------------------------------------------------------------------------------------------

    /**
     * GET_NONCE: a fresh nonce, the PIN key and its binding. The binding is ECDSA-SHA256 by the attestation key over
     * "fedimintcard/pin1" || the PIN key (33, compressed), made at the first asking after SET_CERT and kept: its length
     * and the DER are written in one transaction, so a torn first asking leaves no binding (made again) or the whole
     * one. With no certificate the attestation key may still change (ATTEST_INIT), so no binding is given (6985). No PIN.
     * Like any instruction but VERIFY_PIN it ends a stretch of the wait (process).
     */
    private void getNonce(APDU apdu, byte[] buf) {
        if (buf[ISO7816.OFFSET_P1] != SEALED || buf[ISO7816.OFFSET_P2] != 0) {
            ISOException.throwIt(ISO7816.SW_WRONG_P1P2);
        }
        if (cert[0] == 0) {
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        if (apdu.setIncomingAndReceive() != 0) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        short keyAt = NONCE_LEN;
        short sigAt = (short) (keyAt + 33);
        try {
            short w = ((ECPublicKey) pinKey.getPublic()).getW(tmp, (short) 0);
            if (w != E_LEN || tmp[0] != (byte) 0x04) {
                ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
            }
            buf[keyAt] = (byte) (0x02 | (tmp[64] & 0x01));
            Util.arrayCopyNonAtomic(tmp, (short) 1, buf, (short) (keyAt + 1), (short) 32);
        } finally {
            Util.arrayFillNonAtomic(tmp, (short) 0, (short) tmp.length, (byte) 0);
        }
        short sl = (short) (pinKeySig[0] & 0xFF);
        if (sl == 0) {
            // label (17) || the PIN key (33), assembled from byte 128 of the buffer, past the answer's 121
            short m = (short) 128;
            Util.arrayCopyNonAtomic(PIN_LABEL, (short) 0, buf, m, (short) 17);
            Util.arrayCopyNonAtomic(buf, keyAt, buf, (short) (m + 17), (short) 33);
            ecdsa.init(attest.getPrivate(), Signature.MODE_SIGN);
            sl = ecdsa.sign(buf, m, (short) 50, buf, sigAt);
            if (sl < 8 || sl > 72) {
                ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
            }
            JCSystem.beginTransaction();
            Util.arrayCopy(buf, sigAt, pinKeySig, (short) 1, sl);
            pinKeySig[0] = (byte) sl;
            JCSystem.commitTransaction();
        } else {
            Util.arrayCopyNonAtomic(pinKeySig, (short) 1, buf, sigAt, sl);
        }
        schnorr.random(nonce, (short) 0, NONCE_LEN);
        held[H_NONCE] = 1;
        Util.arrayCopyNonAtomic(nonce, (short) 0, buf, (short) 0, NONCE_LEN);
        apdu.setOutgoingAndSend((short) 0, (short) (sigAt + sl));
    }

    /**
     * A sealed command's data, `len` bytes at OFFSET_CDATA (E || ct || tag), opened in place: the clear data is left at
     * OFFSET_CDATA and its length returned, or -1 where the envelope does not open (E no point of the curve, or the tag
     * not this sum's). The nonce is spent here, opened or not: one nonce opens at most one envelope. E is held to the
     * curve ({@link #requireFullPoint}) before it reaches the key agreement, and the tag is checked before anything is
     * opened. The INS is the command's own, so an envelope sealed for one command does not open as another.
     */
    private short unseal(byte[] buf, short len) {
        if ((short) buf.length < FIELD_END) {
            // the curve check's field room would run past the buffer
            ISOException.throwIt(Secp256k1.SW_CRYPTO_ERROR);
        }
        held[H_NONCE] = 0;
        short e = ISO7816.OFFSET_CDATA;
        short n = (short) (len - E_LEN - TAG_LEN);
        short ct = (short) (e + E_LEN);
        short tag = (short) (ct + n);
        boolean good = false;
        try {
            requireFullPoint(buf, e);
            // after the curve check, so its working room cannot reach it
            x[S_INS] = buf[ISO7816.OFFSET_INS];
            short got = schnorr.agree((ECPrivateKey) pinKey.getPrivate(), buf, e, E_LEN, x, S_POINT);
            if (got == E_LEN && x[S_POINT] == (byte) 0x04) {
                sealBlock(buf, e, (byte) 0, buf, ct, n);
                good = sameBytes(x, S_HASH, buf, tag, TAG_LEN);
            }
        } catch (RuntimeException ex) {
            // Whatever the curve check or the key agreement throws for a point that is no point, the envelope does
            // not open: it is not an error of the card's.
            good = false;
        }
        if (!good) {
            return (short) -1;
        }
        byte i = 1;
        for (short at = 0; at < n; at += (short) 32) {
            sealBlock(buf, e, i, buf, (short) 0, (short) 0);
            for (short k = 0; k < (short) 32 && (short) (at + k) < n; k++) {
                buf[(short) (ct + at + k)] ^= x[(short) (S_HASH + k)];
            }
            i++;
        }
        moveDown(buf, ct, e, n);
        return n;
    }

    /**
     * One block of a sealed command's sum, into x[S_HASH]: SHA-256("fedimintcard/seal1" || i (1) || the shared X (32) ||
     * E (65) || the nonce (16) || the INS (1) || more). The shared point is in x[S_POINT], the INS in x[S_INS].
     */
    private void sealBlock(byte[] buf, short e, byte i, byte[] more, short moreAt, short moreLen) {
        x[S_I] = i;
        sha.reset();
        sha.update(SEAL_LABEL, (short) 0, (short) SEAL_LABEL.length);
        sha.update(x, S_I, (short) 1);
        sha.update(x, (short) (S_POINT + 1), (short) 32);
        sha.update(buf, e, E_LEN);
        sha.update(nonce, (short) 0, NONCE_LEN);
        sha.update(x, S_INS, (short) 1);
        sha.doFinal(more, moreAt, moreLen, x, S_HASH);
    }

    /**
     * The PIN block at buf[at] (its length, the PIN, zeros to eight) taken apart: the PIN moved down to `at` and its
     * length returned; 6700 for a length that is not 4..8.
     */
    private static short unblockPin(byte[] buf, short at) {
        short len = (short) (buf[at] & 0xFF);
        if (len < PIN_MIN || len > PIN_MAX) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        moveDown(buf, (short) (at + 1), at, len);
        return len;
    }

    /**
     * An envelope that did not open, in VERIFY_PIN or CHANGE_PIN: a wrong try, as in Cashu's applet, so every
     * envelope counts against the PIN's tries. Through {@link #tryPin}, so the try is counted with its wait
     * before the compare and this selection's PIN session ends (the first SET_PIN's included). The PIN is shown eight
     * random bytes.
     */
    private void openingFailed(byte[] buf) {
        schnorr.random(buf, ISO7816.OFFSET_CDATA, (short) 8);
        tryPin(buf, (short) 8);
        // eight random bytes that were the PIN: the envelope still did not open
        pin.reset();
        ISOException.throwIt((short) (0x63C0 | pinMarks[PM_WRONG]));
    }

    /** The seal's room and the curve check's: nothing of a sealed command outlives it. */
    private void forgetSeal() {
        Util.arrayFillNonAtomic(x, (short) 0, X_LEN, (byte) 0);
        Util.arrayFillNonAtomic(tmp, (short) 0, (short) tmp.length, (byte) 0);
    }

    /**
     * 6A80 unless the 65 bytes at a[at] are a point of the curve in the 04 form: x and y under p, x^3 + 7 a square, and
     * y one of its two roots. The field room is the APDU buffer's [FIELD, FIELD_END), x^3 + 7 is kept in tmp[0..32)
     * between the two powers, and reduceModP works in x[X_RED..X_RED + 40).
     */
    private void requireFullPoint(byte[] a, short at) {
        short py = (short) (at + 33);
        requireFullForm(a, at);
        curveSide(a, (short) (at + 1), a, FIELD);
        Util.arrayCopyNonAtomic(a, (short) (FIELD + 96), tmp, (short) 0, (short) 32);
        Util.arrayCopyNonAtomic(a, (short) (FIELD + 96), a, (short) (FIELD + 32), (short) 32);
        if (!isSquareModP(a, FIELD)) {
            ISOException.throwIt(ISO7816.SW_WRONG_DATA);
        }
        Util.arrayCopyNonAtomic(tmp, (short) 0, a, (short) (FIELD + 32), (short) 32);
        powModP(rsaRoot, a, FIELD);
        if ((short) (a[(short) (FIELD + 127)] & 1) != (short) (a[(short) (py + 31)] & 1)) {
            negateModP(a, (short) (FIELD + 96));
        }
        if (!sameBytes(a, (short) (FIELD + 96), a, py, (short) 32)) {
            ISOException.throwIt(ISO7816.SW_WRONG_DATA);
        }
    }

    /** 6A80 unless the 65 bytes at a[at] are 04, then an x under p and a y under p. */
    private static void requireFullForm(byte[] a, short at) {
        if (a[at] != (byte) 0x04
            || SchnorrHW.cmp32(a, (short) (at + 1), Secp256k1.SECP256K1_P, (short) 0) >= 0
            || SchnorrHW.cmp32(a, (short) (at + 33), Secp256k1.SECP256K1_P, (short) 0) >= 0) {
            ISOException.throwIt(ISO7816.SW_WRONG_DATA);
        }
    }

    /** x^3 + 7 mod p, for the x at a[xAt] (32 bytes, under p), into room[off + 96 .. off + 128): 128 bytes of room. */
    private void curveSide(byte[] a, short xAt, byte[] room, short off) {
        Util.arrayFillNonAtomic(room, off, (short) 32, (byte) 0);
        Util.arrayCopyNonAtomic(a, xAt, room, (short) (off + 32), (short) 32);
        powModP(rsaCube, room, off);
        add7ModP(room, (short) (off + 96));
    }

    /** Euler's test of the value at room[off + 32 .. off + 64): true where it is a square mod p and not zero. */
    private boolean isSquareModP(byte[] room, short off) {
        powModP(rsaEuler, room, off);
        return isZero(room, (short) (off + 96), (short) 31) && room[(short) (off + 127)] == (byte) 1;
    }

    /**
     * The power of the 64 bytes at a[off] by the chip's RSA mod p*p, into a[off + 64 .. off + 128), then taken mod p:
     * the answer is the last 32 of those 64.
     */
    private void powModP(RSAPrivateKey key, byte[] a, short off) {
        rsa.init(key, Cipher.MODE_DECRYPT);
        short out = (short) (off + 64);
        short n = rsa.doFinal(a, off, (short) 64, a, out);
        if (n < (short) 64) {
            // an answer without its leading zeros (the simulator's): set right-aligned
            Util.arrayCopyNonAtomic(a, out, a, (short) (out + 64 - n), n);
            Util.arrayFillNonAtomic(a, out, (short) (64 - n), (byte) 0);
        }
        reduceModP(a, out, x, X_RED);
    }

    /** `k` made the 512-bit RSA key of one power mod p*p, exponent d. */
    private static void fieldKey(RSAPrivateKey k, byte[] d) {
        k.setModulus(RSA_PP, (short) 0, (short) 64);
        k.setExponent(d, (short) 0, (short) d.length);
    }

    /**
     * The 64-byte number at v[off] taken mod p, in place (the answer in its last 32 bytes). 2^256 is 2^32 + 977 mod p,
     * so the high half times that, added to the low half, is the same number mod p and 223 bits shorter; twice over it
     * is under 2^257, and then p is taken off while it is p or more. `w`: 40 bytes of working room.
     */
    private static void reduceModP(byte[] v, short off, byte[] w, short wOff) {
        for (short pass = 0; pass < 2; pass++) {
            Util.arrayFillNonAtomic(w, wOff, (short) 40, (byte) 0);
            Util.arrayCopyNonAtomic(v, (short) (off + 32), w, (short) (wOff + 8), (short) 32);
            mulAddByte(w, (short) (wOff + 36), v, off, (short) 32, (byte) 0x01);
            mulAddByte(w, (short) (wOff + 40), v, off, (short) 32, (byte) 0xD1);
            mulAddByte(w, (short) (wOff + 39), v, off, (short) 32, (byte) 0x03);
            Util.arrayFillNonAtomic(v, off, (short) 24, (byte) 0);
            Util.arrayCopyNonAtomic(w, wOff, v, (short) (off + 24), (short) 40);
        }
        while (!isZero(v, off, (short) 32) || SchnorrHW.cmp32(v, (short) (off + 32), Secp256k1.SECP256K1_P, (short) 0) >= 0) {
            subP64(v, off);
        }
        Util.arrayFillNonAtomic(w, wOff, (short) 40, (byte) 0);
    }

    /** w, ending at `wEnd` (exclusive), += a (`len` bytes) times the byte m; the carry runs left as far as it goes. */
    private static void mulAddByte(byte[] w, short wEnd, byte[] a, short aOff, short len, byte m) {
        short mm = (short) (m & 0xFF);
        short carry = 0;
        short pos = (short) (wEnd - 1);
        for (short i = (short) (len - 1); i >= 0; i--, pos--) {
            short prod = (short) ((short) ((short) (a[(short) (aOff + i)] & 0xFF) * mm) + (short) (w[pos] & 0xFF) + carry);
            w[pos] = (byte) (prod & 0xFF);
            carry = (short) ((prod >> 8) & 0xFF);
        }
        while (carry != 0 && pos >= 0) {
            short sum = (short) ((short) (w[pos] & 0xFF) + carry);
            w[pos] = (byte) (sum & 0xFF);
            carry = (short) ((sum >> 8) & 0xFF);
            pos--;
        }
    }

    /** v[off..off+64) -= p, as one 64-byte number. */
    private static void subP64(byte[] v, short off) {
        short borrow = SchnorrHW.sub32(v, (short) (off + 32), Secp256k1.SECP256K1_P, (short) 0, v, (short) (off + 32));
        for (short i = (short) (off + 31); borrow != 0 && i >= off; i--) {
            short d = (short) ((short) (v[i] & 0xFF) - borrow);
            if (d < 0) {
                d += (short) 256;
                borrow = 1;
            } else {
                borrow = 0;
            }
            v[i] = (byte) d;
        }
    }

    /** The 32 bytes at off, under p: += 7, mod p. */
    private static void add7ModP(byte[] v, short off) {
        short carry = 7;
        for (short i = 31; i >= 0 && carry != 0; i--) {
            short sum = (short) ((short) (v[(short) (off + i)] & 0xFF) + carry);
            v[(short) (off + i)] = (byte) (sum & 0xFF);
            carry = (short) ((sum >> 8) & 0xFF);
        }
        if (SchnorrHW.cmp32(v, off, Secp256k1.SECP256K1_P, (short) 0) >= 0) {
            SchnorrHW.sub32(v, off, Secp256k1.SECP256K1_P, (short) 0, v, off);
        }
    }

    /** The 32 bytes at off, under p and not zero: p - v. */
    private static void negateModP(byte[] v, short off) {
        SchnorrHW.sub32(Secp256k1.SECP256K1_P, (short) 0, v, off, v, off);
    }

    /** `len` bytes moved from `from` down to `to` (to < from), a byte at a time: the two ranges may overlap. */
    private static void moveDown(byte[] buf, short from, short to, short len) {
        for (short k = 0; k < len; k++) {
            buf[(short) (to + k)] = buf[(short) (from + k)];
        }
    }

    /** Whether `len` bytes agree; every byte is read, whatever the first difference. */
    private static boolean sameBytes(byte[] a, short aOff, byte[] b, short bOff, short len) {
        byte d = 0;
        for (short k = 0; k < len; k++) {
            d |= (byte) (a[(short) (aOff + k)] ^ b[(short) (bOff + k)]);
        }
        return d == 0;
    }
}
