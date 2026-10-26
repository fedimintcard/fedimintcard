package io.github.bobodread876.fedimintcard.applet;

import javacard.framework.APDU;
import javacard.framework.Applet;
import javacard.framework.ISO7816;
import javacard.framework.ISOException;
import javacard.framework.JCSystem;
import javacard.framework.Util;
import javacard.security.ECPrivateKey;
import javacard.security.ECPublicKey;
import javacard.security.KeyBuilder;
import javacard.security.KeyPair;

/**
 * Fedimint spend-key applet (spike). Holds up to {@link #SLOTS} secp256k1 keys
 * and signs 32-byte messages with BIP340 Schnorr. No command ever returns a
 * private key.
 *
 * <pre>
 * CLA B0
 * 10 GEN_KEY  P1=slot  P2=0|1 Lc=0   -> 32 bytes x-only pubkey (P2=1 overwrites a used slot)
 * 11 GET_PUB  P1=slot  Lc=0         -> 32 bytes x-only pubkey
 * 12 SIGN     P1=slot  Lc=32 (msg)  -> 64 bytes R.x || s
 * </pre>
 */
public final class FedimintCardApplet extends Applet {

    static final byte CLA = (byte) 0xB0;
    static final byte INS_GEN_KEY = (byte) 0x10;
    static final byte INS_GET_PUB = (byte) 0x11;
    static final byte INS_SIGN = (byte) 0x12;
    static final short SLOTS = 8;

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
    private final SchnorrHW schnorr;

    private FedimintCardApplet() {
        for (short i = 0; i < SLOTS; i++) {
            privs[i] = (ECPrivateKey) KeyBuilder.buildKey(
                KeyBuilder.TYPE_EC_FP_PRIVATE, KeyBuilder.LENGTH_EC_FP_256, false);
            pubs[i] = (ECPublicKey) KeyBuilder.buildKey(
                KeyBuilder.TYPE_EC_FP_PUBLIC, KeyBuilder.LENGTH_EC_FP_256, false);
            setParams(privs[i], pubs[i]);
        }
        gen = new KeyPair(KeyPair.ALG_EC_FP, KeyBuilder.LENGTH_EC_FP_256);
        setParams((ECPrivateKey) gen.getPrivate(), (ECPublicKey) gen.getPublic());
        tmp = JCSystem.makeTransientByteArray((short) 65, JCSystem.CLEAR_ON_DESELECT);
        schnorr = new SchnorrHW(G, P, A, B, N);
        schnorr.init();
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

    @Override
    public void process(APDU apdu) {
        if (selectingApplet()) {
            return;
        }
        byte[] buf = apdu.getBuffer();
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
            default:
                ISOException.throwIt(ISO7816.SW_INS_NOT_SUPPORTED);
        }
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
            setParams(privs[s], null);
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
