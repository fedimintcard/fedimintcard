package io.github.bobodread876.fedimintcard.applet;

import javacard.framework.APDU;
import javacard.framework.Applet;
import javacard.framework.ISO7816;
import javacard.framework.ISOException;
import javacard.framework.JCSystem;
import javacard.framework.OwnerPIN;
import javacard.framework.Util;
import javacard.security.ECPrivateKey;
import javacard.security.ECPublicKey;
import javacard.security.KeyBuilder;
import javacard.security.KeyPair;

/**
 * Fedimint spend-key applet (spike). Holds up to {@link #SLOTS} secp256k1 keys
 * and signs 32-byte messages with BIP340 Schnorr once a PIN has been given. No
 * command ever returns a private key.
 *
 * <pre>
 * CLA B0
 * 10 GEN_KEY  P1=slot  P2=0|1 Lc=0    -> 32 bytes x-only pubkey (P2=1 overwrites a used slot; after VERIFY_PIN once a PIN is set)
 * 11 GET_PUB  P1=slot  Lc=0          -> 32 bytes x-only pubkey
 * 12 SIGN     P1=slot  Lc=32 (msg)   -> 64 bytes R.x || s   (after VERIFY_PIN)
 * 40 VERIFY_PIN  P1=P2=0 Lc=4..8      -> the session may SIGN
 * 41 SET_PIN     P1=P2=0 Lc=4..8      -> once per card; does not open a session
 * 42 CHANGE_PIN  P1=P2=0 Lc=8..16     -> after VERIFY_PIN; closes the session
 * </pre>
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
    static final byte INS_VERIFY_PIN = (byte) 0x40;
    static final byte INS_SET_PIN = (byte) 0x41;
    static final byte INS_CHANGE_PIN = (byte) 0x42;
    static final short SLOTS = 8;

    /** SELECT answers this major and {@link #VERSION_MINOR}; see the class javadoc. */
    static final byte VERSION_MAJOR = 0x02;
    static final byte VERSION_MINOR = 0x00;

    /** PIN size bounds (bytes; the applet never interprets them) and the retry limit. */
    static final byte PIN_MIN = 4;
    static final byte PIN_MAX = 8;
    static final byte PIN_TRIES = 3;

    static final short SW_PIN_BLOCKED = 0x6983;
    static final short SW_PIN_NOT_SET = 0x6984;
    static final short SW_PIN_NEEDED = 0x6A94;

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
    private final boolean[] used = new boolean[SLOTS];
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

    private FedimintCardApplet() {
        for (short i = 0; i < SLOTS; i++) {
            privs[i] = (ECPrivateKey) KeyBuilder.buildKey(
                KeyBuilder.TYPE_EC_FP_PRIVATE, KeyBuilder.LENGTH_EC_FP_256, false);
            pubs[i] = (ECPublicKey) KeyBuilder.buildKey(
                KeyBuilder.TYPE_EC_FP_PUBLIC, KeyBuilder.LENGTH_EC_FP_256, false);
            // Domain parameters are written at GEN_KEY, not here: the J3R180 runs the
            // constructor inside a bounded commit buffer and refuses INSTALL (6F00) when
            // more slots' parameters are written at once (16 slots failed, 8 passed).
        }
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
            case INS_VERIFY_PIN:
                verifyPin(apdu, buf);
                break;
            case INS_SET_PIN:
                setPin(apdu, buf);
                break;
            case INS_CHANGE_PIN:
                changePin(apdu, buf);
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

    private void genKey(APDU apdu, byte[] buf) {
        short s = slot(buf, true);
        boolean overwrite = buf[ISO7816.OFFSET_P2] == 1;
        if (apdu.setIncomingAndReceive() != 0) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        if (used[s]) {
            if (!overwrite) {
                ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
            }
            // Replacing a key destroys whatever notes were issued to it, so once the card has a PIN it
            // takes the same PIN as spending them: without this, any reader in range could wipe a slot
            // with one APDU. Before the first SET_PIN nothing can be spent, so provisioning may still
            // re-key freely; a blocked card refuses (69 83).
            if (pinState == PIN_STATE_SET || pinState == PIN_STATE_BLOCKED) {
                checkPinReady();
            }
            // The old key is being replaced: wipe it first so a failure below leaves the slot empty.
            used[s] = false;
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
        used[s] = true;
        sendPub(apdu, buf, s);
    }

    private void getPub(APDU apdu, byte[] buf) {
        short s = slot(buf, false);
        if (apdu.setIncomingAndReceive() != 0) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        if (!used[s]) {
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        sendPub(apdu, buf, s);
    }

    private void sign(APDU apdu, byte[] buf) {
        // The gate comes first: without the PIN, a reader learns nothing about the slots.
        checkPinReady();
        short s = slot(buf, false);
        short got = apdu.setIncomingAndReceive();
        if (got != 32 || apdu.getIncomingLength() != 32) {
            ISOException.throwIt(ISO7816.SW_WRONG_LENGTH);
        }
        if (!used[s]) {
            ISOException.throwIt(ISO7816.SW_CONDITIONS_NOT_SATISFIED);
        }
        schnorr.sign(privs[s], pubs[s], buf, ISO7816.OFFSET_CDATA, buf, (short) 0);
        apdu.setOutgoingAndSend((short) 0, (short) 64);
    }
}
