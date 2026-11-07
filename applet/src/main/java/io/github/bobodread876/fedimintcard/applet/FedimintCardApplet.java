package io.github.bobodread876.fedimintcard.applet;

import javacard.framework.APDU;
import javacard.framework.Applet;
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
import javacard.security.Signature;

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
 *                                        PIN is set); a slot holding a note or owed change is never re-keyed (6985)
 * 11 GET_PUB     P1=slot Lc=0         -> 32 bytes x-only pubkey
 * 12 SIGN        P1=slot Lc=32 (hash) -> 64 bytes R.x || s (after VERIFY_PIN). A NOTE slot: counted against the day's limit,
 *                                        then spent. A SPENT slot: the same hash only. Else 6985; over the limit 6A95
 * 13 STATE_KEY   P1=P2=0              -> 32 bytes (after VERIFY_PIN)
 * 14 SET_NOTE    P1=slot Lc=56        amount (8, msat, big-endian) || federation signature (48). A KEYED slot, or a CHANGE
 *                                        slot finished (same amount); the slot becomes NOTE
 * 15 GET_SLOT    P1=slot              -> state (1) || x-only pubkey (32) || amount and signature (56) || spent hash (32)
 *                                        || opening (66)
 * 16 GET_STATE                        -> slot states (32) || now (4) || window (4) || limit (8) || spent today (8)
 *                                        || certificate length (1) || attestation key present (1)
 * 17 SET_CHANGE  P1=slot Lc=74        amount (8) || txid (32) || output index (2) || blinding key (32). A KEYED slot becomes CHANGE
 * 18 SET_LIMIT   Lc=8                 the day's limit in msat (0: none). Only before SET_PIN
 * 19 SET_HEADER  Lc=80                a Bitcoin block header -> now (4). Moves the clock if later, with enough work (6A93)
 * 30 ATTEST_INIT                      -> P-256 public key (65). Makes the attestation key; only before SET_PIN and the certificate
 * 31 SET_CERT    Lc=1..160            the registry's certificate over this card; once, before SET_PIN
 * 32 GET_CERT                         -> the certificate
 * 33 ATTEST      Lc=32 (challenge)    -> ECDSA-SHA256 (DER) over "fedimintcard/at1" || challenge || slot 0's x-only pubkey
 * 40 VERIFY_PIN  P1=P2=0 Lc=4..8      -> the session may SIGN
 * 41 SET_PIN     P1=P2=0 Lc=4..8      -> once per card; does not open a session
 * 42 CHANGE_PIN  P1=P2=0 Lc=8..16     -> after VERIFY_PIN; closes the session
 * </pre>
 *
 * <p>Slot states: EMPTY 0, KEYED 1 (a key, no note), NOTE 2, CHANGE 3 (a key owed change), SPENT 4.
 * The day's limit counts what a note signs away whole: the card cannot see a transaction's
 * outputs, so change coming back is not netted off (cashucard nets it; its card makes its own
 * change output). Amounts are the terminal's word, as in cashucard: the card cannot check a BLS
 * signature, the till does.
 *
 * <p>PIN status words: {@code 69 83} blocked, {@code 69 84} no PIN set yet,
 * {@code 6A 94} a PIN is set and this session has not verified it, {@code 63 CX}
 * a mismatch with X tries left. {@code SET_PIN} on a card that already has a PIN
 * answers {@code 69 85}. A blocked card answers {@code 69 83} to every PIN
 * command, whatever the frame: the state is checked before P1/P2 and Lc there.
 *
 * <p>{@code SELECT} answers two data bytes with {@code 90 00}: major version, then
 * minor. This is the applet's version alone — no card-manager status byte, no
 * FCI — so the first byte of the answer is the version. See {@link #VERSION_MAJOR}.
 *
 * <p>The verified flag is {@code CLEAR_ON_DESELECT} transient: a session is one
 * applet selection, and a card reset or a SELECT of another AID ends it. The PIN
 * itself and its retry counter live in {@link OwnerPIN}, which spends one try in
 * {@code check()} even on success and puts it back — that is what {@code 63CX}
 * reports, and why a wrong picture of the counter is a security bug rather than
 * a cosmetic one.
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
    static final short SLOTS = 32;

    /** SELECT answers this major and {@link #VERSION_MINOR}; see the class javadoc. */
    static final byte VERSION_MAJOR = 0x03;
    static final byte VERSION_MINOR = 0x00;

    /** PIN size bounds (bytes; the applet never interprets them) and the retry limit. */
    static final byte PIN_MIN = 4;
    static final byte PIN_MAX = 8;
    static final byte PIN_TRIES = 3;

    static final short SW_PIN_BLOCKED = 0x6983;
    static final short SW_PIN_NOT_SET = 0x6984;
    static final short SW_PIN_NEEDED = 0x6A94;
    static final short SW_OVER_LIMIT = 0x6A95;
    static final short SW_LITTLE_WORK = 0x6A93;

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

    static final byte ST_EMPTY = 0;
    static final byte ST_KEYED = 1;
    static final byte ST_NOTE = 2;
    static final byte ST_CHANGE = 3;
    static final byte ST_SPENT = 4;

    /** A note: its amount in msat (8, big-endian), then the federation's BLS signature (48). */
    static final short NOTE_LEN = 56;
    /** Change owed to a slot: the transaction (32), the output's index (2), its blinding key (32). */
    static final short OPEN_LEN = 66;
    static final short CERT_MAX = 160;
    static final short HEADER_LEN = 80;

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
    /** ATTEST's label: ASCII "fedimintcard/at1", 16 bytes. */
    private static final byte[] ATTEST_LABEL = {
        'f','e','d','i','m','i','n','t','c','a','r','d','/','a','t','1' };

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
     * byte is written after the PIN it describes, so a tear reads as not set (or blocked), never
     * as set with a PIN nobody knows.
     */
    static final byte PIN_STATE_NOT_SET = 0;
    static final byte PIN_STATE_SET = 1;
    static final byte PIN_STATE_BLOCKED = 2;

    private static final byte[] P = {
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFE,
        (byte)0xFF,(byte)0xFF,(byte)0xFC,(byte)0x2F };
    private static final byte[] A = new byte[32];
    private static final byte[] B = {
        0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,7 };
    private static final byte[] N = {
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,
        (byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFE,
        (byte)0xBA,(byte)0xAE,(byte)0xDC,(byte)0xE6,(byte)0xAF,(byte)0x48,(byte)0xA0,(byte)0x3B,
        (byte)0xBF,(byte)0xD2,(byte)0x5E,(byte)0x8C,(byte)0xD0,(byte)0x36,(byte)0x41,(byte)0x41 };
    private static final byte[] G = {
        (byte)0x04,
        (byte)0x79,(byte)0xBE,(byte)0x66,(byte)0x7E,(byte)0xF9,(byte)0xDC,(byte)0xBB,(byte)0xAC,
        (byte)0x55,(byte)0xA0,(byte)0x62,(byte)0x95,(byte)0xCE,(byte)0x87,(byte)0x0B,(byte)0x07,
        (byte)0x02,(byte)0x9B,(byte)0xFC,(byte)0xDB,(byte)0x2D,(byte)0xCE,(byte)0x28,(byte)0xD9,
        (byte)0x59,(byte)0xF2,(byte)0x81,(byte)0x5B,(byte)0x16,(byte)0xF8,(byte)0x17,(byte)0x98,
        (byte)0x48,(byte)0x3A,(byte)0xDA,(byte)0x77,(byte)0x26,(byte)0xA3,(byte)0xC4,(byte)0x65,
        (byte)0x5D,(byte)0xA4,(byte)0xFB,(byte)0xFC,(byte)0x0E,(byte)0x11,(byte)0x08,(byte)0xA8,
        (byte)0xFD,(byte)0x17,(byte)0xB4,(byte)0x48,(byte)0xA6,(byte)0x85,(byte)0x54,(byte)0x19,
        (byte)0x9C,(byte)0x47,(byte)0xD0,(byte)0x8F,(byte)0xFB,(byte)0x10,(byte)0xD4,(byte)0xB8 };

    private final ECPrivateKey[] privs = new ECPrivateKey[SLOTS];
    private final ECPublicKey[] pubs = new ECPublicKey[SLOTS];
    private final byte[] state = new byte[SLOTS];
    private final byte[] notes = new byte[(short) (SLOTS * NOTE_LEN)];
    private final byte[] spentHash = new byte[(short) (SLOTS * 32)];
    private final byte[] openings = new byte[(short) (SLOTS * OPEN_LEN)];
    private final byte[] clock = new byte[CK_LEN];
    /** The certificate: its length (1), then up to {@link #CERT_MAX} bytes. */
    private final byte[] cert = new byte[(short) (CERT_MAX + 1)];
    /** The attestation key pair, built at install; its key is made at ATTEST_INIT. */
    private final KeyPair attest;
    /** Whether ATTEST_INIT has made the attestation key. */
    private boolean attestMade;
    private final Signature ecdsa;
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
    private final KeyPair gen;
    private final byte[] tmp;
    /**
     * CHANGE_PIN's new half, persistent (not cleared on deselect: it is zeroed at the end of every
     * call that writes it). A transient array would be wiped by {@link #checkPin}'s cleanup, which
     * clears {@link #tmp} along with the APDU buffer before the new PIN is written.
     */
    private final byte[] newPin;
    private final SchnorrHW schnorr;
    private final OwnerPIN pin;
    private byte pinState;
    /**
     * The card secret behind STATE_KEY: 32 random bytes made at install, kept in a key object so
     * the chip protects it at rest like the slot keys. It never leaves the card; only the HMAC
     * derived from it does.
     */
    private final AESKey secret;

    private FedimintCardApplet() {
        // A slot's key objects are built at its first GEN_KEY (keysFor), not here: the J3R180
        // runs the constructor inside a bounded commit buffer and refuses INSTALL (6F00) when it
        // builds too much at once (16 slots' keys failed, 8 passed).
        gen = new KeyPair(KeyPair.ALG_EC_FP, KeyBuilder.LENGTH_EC_FP_256);
        setParams((ECPrivateKey) gen.getPrivate(), (ECPublicKey) gen.getPublic());
        tmp = JCSystem.makeTransientByteArray((short) 65, JCSystem.CLEAR_ON_DESELECT);
        // Install-time only, and persistent: see the field's comment.
        newPin = new byte[PIN_MAX];
        schnorr = new SchnorrHW(G, P, A, B, N);
        schnorr.init();
        // Install-time only: OwnerPIN and the state byte are the applet's persistent PIN store.
        // A new instance starts on the not-set path, as the check in set() expects.
        pin = new OwnerPIN(PIN_TRIES, PIN_MAX);
        pinState = PIN_STATE_NOT_SET;
        attest = new KeyPair(KeyPair.ALG_EC_FP, KeyBuilder.LENGTH_EC_FP_256);
        p256((javacard.security.ECKey) attest.getPrivate());
        p256((javacard.security.ECKey) attest.getPublic());
        ecdsa = Signature.getInstance(Signature.ALG_ECDSA_SHA_256, false);
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
        if (priv != null) setPrivParams(priv);
        if (pub != null) setPubParams(pub);
    }

    private static void setPrivParams(ECPrivateKey priv) {
        priv.setFieldFP(P, (short) 0, (short) 32);
        priv.setA(A, (short) 0, (short) 32);
        priv.setB(B, (short) 0, (short) 32);
        priv.setG(G, (short) 0, (short) 65);
        priv.setR(N, (short) 0, (short) 32);
        priv.setK((short) 1);
    }

    private static void setPubParams(ECPublicKey pub) {
        pub.setFieldFP(P, (short) 0, (short) 32);
        pub.setA(A, (short) 0, (short) 32);
        pub.setB(B, (short) 0, (short) 32);
        pub.setG(G, (short) 0, (short) 65);
        pub.setR(N, (short) 0, (short) 32);
        pub.setK((short) 1);
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
        pin.reset();
        return true;
    }

    @Override
    public void deselect() {
        pin.reset();
    }

    @Override
    public void process(APDU apdu) {
        byte[] buf = apdu.getBuffer();
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
            default:
                ISOException.throwIt(ISO7816.SW_INS_NOT_SUPPORTED);
        }
    }

    /**
     * The gate on SIGN, checked before the slot and the message so a reader without the PIN
     * cannot probe which slots are keyed.
     */
    private void checkPinReady() {
        if (pinState == PIN_STATE_BLOCKED) {
            ISOException.throwIt(SW_PIN_BLOCKED);
        }
        if (pinState != PIN_STATE_SET) {
            ISOException.throwIt(SW_PIN_NOT_SET);
        }
        if (!pin.isValidated()) {
            ISOException.throwIt(SW_PIN_NEEDED);
        }
    }

    /**
     * One PIN attempt: P1 and P2 must be 0 and the data field {@code len} bytes with
     * {@code PIN_MIN <= len <= PIN_MAX}, else {@code 6700} without spending a try. A blocked PIN
     * answers {@code 6983} whatever is sent. A mismatch spends a try and answers {@code 63C0|left};
     * the third spends none and leaves the card blocked with {@code 6983}.
     *
     * <p>A success returns with the PIN validated for this session and the retry counter restored.
     * Every path wipes the APDU buffer and {@link #tmp}, so no PIN byte outlives the call.
     *
     * <p>{@code len} is the Lc that {@link #lcOf} read. Only the {@code len} bytes of the old PIN
     * are checked, whichever is longer: CHANGE_PIN sends its new PIN after them and that half is
     * not part of the check.
     *
     * <p>The data must already be in the buffer: {@code setIncomingAndReceive} may be called only
     * once per {@code process()}, so the caller receives it and this only compares.
     */
    private void checkPinReceived(byte[] buf, short len) {
        if (buf[ISO7816.OFFSET_P1] != 0 || buf[ISO7816.OFFSET_P2] != 0) {
            ISOException.throwIt(ISO7816.SW_WRONG_P1P2);
        }
        if (pinState == PIN_STATE_BLOCKED || pin.getTriesRemaining() == 0) {
            // The try the counter already spent on the previous call: commit the block, then refuse.
            pinState = PIN_STATE_BLOCKED;
            ISOException.throwIt(SW_PIN_BLOCKED);
        }
        // Checked before the data is read, so a wrong length costs neither a try nor a read.
        if (len < PIN_MIN || len > PIN_MAX) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        try {
            if (pin.check(buf, ISO7816.OFFSET_CDATA, (byte) len)) {
                return;
            }
        } finally {
            Util.arrayFillNonAtomic(buf, (short) 0, (short) buf.length, (byte) 0);
            Util.arrayFillNonAtomic(tmp, (short) 0, (short) tmp.length, (byte) 0);
        }
        short left = pin.getTriesRemaining();
        if (left == 0) {
            pinState = PIN_STATE_BLOCKED;
            ISOException.throwIt(SW_PIN_BLOCKED);
        }
        ISOException.throwIt((short) (0x63C0 | left));
    }

    /** Opens this session for SIGN. Also starts a fresh one, so a wrong PIN leaves nothing open. */
    private void verifyPin(APDU apdu, byte[] buf) {
        short len = lcOf(buf);
        if (pinState == PIN_STATE_BLOCKED) {
            // Blocked outranks not-set: a card that ran out of tries must say so, or the holder
            // would be told no PIN exists when the truth is that the card is dead.
            ISOException.throwIt(SW_PIN_BLOCKED);
        }
        if (pinState != PIN_STATE_SET) {
            ISOException.throwIt(SW_PIN_NOT_SET);
        }
        if (len < PIN_MIN || len > PIN_MAX) {
            // Refused before any receive, so a wrong length costs neither a try nor a read.
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        if (apdu.setIncomingAndReceive() < len) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        checkPinReceived(buf, len);
    }

    /**
     * The one-time PIN write. Does not open a session: the terminal verifies to prove it took.
     *
     * <p>A blocked PIN answers {@code 6983} whether or not the frame is well formed: the counter
     * has nothing left to protect, and the state is the answer the terminal needs. So the state
     * is checked before P1/P2 and Lc, unlike on a usable PIN, where the frame comes first.
     */
    private void setPin(APDU apdu, byte[] buf) {
        short len = lcOf(buf);
        if (pinState == PIN_STATE_BLOCKED) {
            ISOException.throwIt(SW_PIN_BLOCKED);
        }
        if (buf[ISO7816.OFFSET_P1] != 0 || buf[ISO7816.OFFSET_P2] != 0) {
            ISOException.throwIt(ISO7816.SW_WRONG_P1P2);
        }
        if (pinState == PIN_STATE_SET) {
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        if (len < PIN_MIN || len > PIN_MAX) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        try {
            if (apdu.setIncomingAndReceive() < len) {
                ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
            }
            // The same buffer as CHANGE_PIN's new half, which is only live inside a call.
            Util.arrayCopyNonAtomic(buf, ISO7816.OFFSET_CDATA, newPin, (short) 0, len);
            // The PIN write first, the state byte second: the state byte is the commit point, so
            // a tear before it leaves the card reading as not set and SET_PIN can run again.
            pin.update(newPin, (short) 0, (byte) len);
            pinState = PIN_STATE_SET;
        } finally {
            Util.arrayFillNonAtomic(buf, (short) 0, (short) buf.length, (byte) 0);
            Util.arrayFillNonAtomic(newPin, (short) 0, (short) newPin.length, (byte) 0);
        }
    }

    /** Lc = old PIN || new PIN, each 4..8 bytes. Requires this session's VERIFY_PIN, and ends it. */
    private void changePin(APDU apdu, byte[] buf) {
        short lc = lcOf(buf);
        if (pinState == PIN_STATE_BLOCKED) {
            // Blocked outranks everything, as in VERIFY_PIN.
            ISOException.throwIt(SW_PIN_BLOCKED);
        }
        if (pinState != PIN_STATE_SET) {
            // And not-set outranks a bad frame, as in VERIFY_PIN: there is no old PIN to change.
            ISOException.throwIt(SW_PIN_NOT_SET);
        }
        if (buf[ISO7816.OFFSET_P1] != 0 || buf[ISO7816.OFFSET_P2] != 0) {
            ISOException.throwIt(ISO7816.SW_WRONG_P1P2);
        }
        if (!pin.isValidated()) {
            ISOException.throwIt(SW_PIN_NEEDED);
        }
        // An odd Lc cannot be two PINs of the same length, and neither half is bounded otherwise.
        if (lc < 2 * PIN_MIN || lc > 2 * PIN_MAX || (lc & 1) != 0) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        short half = (short) (lc / 2);
        try {
            // The whole data field is received here, before anything reads or wipes the buffer:
            // `setIncomingAndReceive` may be called only once per process() (APDU spec), and on
            // T=0 it "may return less than a full buffer of bytes". Reading only the new half
            // below would copy bytes the card has not received yet — stale buffer content — and
            // make it the new PIN. So take all `lc` bytes first, then split them in newPin.
            //
            // Not covered by a test: jCardSim is T=1 and always delivers the full field, and it
            // answers 6F00 to a command that ends with the APDU still partially received, so the
            // truncation cannot be injected from outside. The check below is the guard.
            if (apdu.setIncomingAndReceive() < lc) {
                ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
            }
            // The new half is copied before the old half's check wipes the APDU buffer and tmp.
            // Into newPin, not tmp, because checkPinReceived wipes tmp on its way out.
            Util.arrayCopyNonAtomic(buf, (short) (ISO7816.OFFSET_CDATA + half), newPin,
                (short) 0, half);
            // The old half goes through the same checks as VERIFY_PIN, comparing exactly the first
            // `half` bytes: the new half that follows is not part of the check. A mismatch spends
            // a try, and a call that finds no try left commits the block and answers 6983.
            checkPinReceived(buf, half);
            // A change ends the session, so the next CHANGE_PIN needs its own VERIFY_PIN.
            pin.reset();
            // The new PIN, then the commit point again: with no transaction primitive on 3.0.5,
            // writing the state byte after the PIN is what makes a tear read as not set.
            pin.update(newPin, (short) 0, (byte) half);
            pinState = PIN_STATE_SET;
        } finally {
            Util.arrayFillNonAtomic(buf, (short) 0, (short) buf.length, (byte) 0);
            Util.arrayFillNonAtomic(newPin, (short) 0, (short) newPin.length, (byte) 0);
        }
    }

    /**
     * Lc from the header in the APDU buffer. Deliberately not {@code APDU.getIncomingLength()}:
     * jCardSim (and the JC 3.0.5 spec) throw {@code APDUException} if incoming data has not been
     * received yet, and every PIN path needs the length to validate the frame before receiving.
     * Only short APDUs are accepted; an extended Lc reads as more than {@code PIN_MAX}, which
     * every caller rejects.
     */
    private static short lcOf(byte[] buf) {
        if (buf[ISO7816.OFFSET_LC] < 0) {
            return (short) (PIN_MAX + 1);
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
            ISOException.throwIt(SchnorrHW.SW_CRYPTO_ERROR);
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
            // re-key freely; a blocked card refuses (69 83).
            if (pinState == PIN_STATE_SET || pinState == PIN_STATE_BLOCKED) {
                checkPinReady();
            }
            // The state byte is the commit point, written alone (a single-byte write is atomic):
            // from here a tear leaves an empty slot, never an old key the card might sign with
            // again, nor a spent hash that a new key would inherit.
            state[s] = ST_EMPTY;
            Util.arrayFillNonAtomic(spentHash, (short) (s * 32), (short) 32, (byte) 0);
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
     * SIGN, at most one transaction per slot (3.0). The signature is made in RAM; the slot's
     * spent hash, its state and the day's count are committed together; only then does the
     * signature leave the card. A tear before the commit has spent nothing and given nothing; a
     * tear after it leaves a spent slot whose one transaction can be signed again.
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
            schnorr.sign(privs[s], pubs[s], buf, ISO7816.OFFSET_CDATA, buf, (short) 0);
            apdu.setOutgoingAndSend((short) 0, (short) 64);
            return;
        }
        if (st != ST_NOTE) {
            // No note to spend: an empty or keyed slot, or one still owed its change.
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        Util.arrayCopyNonAtomic(buf, ISO7816.OFFSET_CDATA, x, X_HASH, (short) 32);
        dayAfter((short) (s * NOTE_LEN));
        schnorr.sign(privs[s], pubs[s], buf, ISO7816.OFFSET_CDATA, buf, (short) 0);
        JCSystem.beginTransaction();
        Util.arrayCopy(x, X_HASH, spentHash, (short) (s * 32), (short) 32);
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
        if (apdu.setIncomingAndReceive() != len || apdu.getIncomingLength() != len) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
    }

    /**
     * SET_NOTE: the note on a KEYED slot, or the finished change on a CHANGE slot (whose amount
     * must match). No PIN: loading spends nothing, as for a holder's load. The note's data and its
     * state are one commit; a CHANGE slot's opening is let go after it.
     */
    private void setNote(APDU apdu, byte[] buf) {
        short s = slot(buf, false);
        receiveExactly(apdu, NOTE_LEN);
        byte st = state[s];
        short n = (short) (s * NOTE_LEN);
        if (st == ST_CHANGE) {
            if (Util.arrayCompare(buf, ISO7816.OFFSET_CDATA, notes, n, (short) 8) != 0) {
                ISOException.throwIt(ISO7816.SW_WRONG_DATA);
            }
        } else if (st != ST_KEYED) {
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        if (isZero(buf, ISO7816.OFFSET_CDATA, (short) 8)) {
            ISOException.throwIt(ISO7816.SW_WRONG_DATA);
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
     * the card, so only this card can spend the change, as in cashucard's GET_CHANGE.
     */
    private void setChange(APDU apdu, byte[] buf) {
        short s = slot(buf, false);
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
                ISOException.throwIt(SchnorrHW.SW_CRYPTO_ERROR);
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
        apdu.setOutgoingAndSend((short) 0, (short) 58);
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
     * SET_HEADER, as cashucard's: a Bitcoin block header moves the card's clock to its time when
     * that is later, if the header has its own bits' work, at least the floor's, and at least a
     * quarter of the hardest the card has taken. A real header exists only once the world has
     * mined it, so no terminal can make the day turn early. Anyone may send one. Answers now (4).
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
        // the block's time, big-endian
        x[X_NUM] = buf[(short) (at + 71)];
        x[(short) (X_NUM + 1)] = buf[(short) (at + 70)];
        x[(short) (X_NUM + 2)] = buf[(short) (at + 69)];
        x[(short) (X_NUM + 3)] = buf[(short) (at + 68)];
        if (cmp(x, X_NUM, clock, CK_NOW, (short) 4) > 0) {
            boolean harder = !seen;
            if (seen) {
                targetOf(clock, CK_BITS, x, X_FLOOR);
                harder = cmp(x, (short) (X_OUT + 32), x, X_FLOOR, (short) 32) < 0;
            }
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
        attest.genKeyPair();
        attestMade = true;
        short n = ((ECPublicKey) attest.getPublic()).getW(buf, (short) 0);
        apdu.setOutgoingAndSend((short) 0, n);
    }

    /** SET_CERT: the registry's certificate over this card's keys, once, before the PIN. */
    private void setCert(APDU apdu, byte[] buf) {
        short len = apdu.setIncomingAndReceive();
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
            ISOException.throwIt(SchnorrHW.SW_CRYPTO_ERROR);
        }
        Util.arrayCopyNonAtomic(tmp, (short) 1, buf, (short) (m + 48), (short) 32);
        ecdsa.init(attest.getPrivate(), Signature.MODE_SIGN);
        short sl = ecdsa.sign(buf, m, (short) 80, buf, (short) 0);
        apdu.setOutgoingAndSend((short) 0, sl);
    }
}
