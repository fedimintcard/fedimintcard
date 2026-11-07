package io.github.bobodread876.fedimintcard.probe;

import javacard.framework.*;
import javacard.security.*;

/**
 * Measures what the offline plan (docs/OFFLINE-PLAN.md, F0) depends on: commit capacity, how many
 * writes one transaction holds, and the time of the native primitives. Timing is taken host-side
 * from APDU round trips (subtract NOOP). Not part of the product CAP.
 *
 *   01 CAPACITY        -> max commit capacity (2), unused (2), free persistent (2, capped at 32767)
 *   02 TXN P1=n P2=s   one transaction of n writes of s bytes (s=0 or 1: a status byte; 32: a hash)
 *                      -> 9000, or 6Axx with the TransactionException reason
 *   03 P256_GEN        genKeyPair on P-256
 *   04 P256_SIGN       ECDSA-SHA256 over 32 bytes with the P-256 key (DER)
 *   05 K1_SIGN         ECDSA-SHA256 over 32 bytes on secp256k1 (explicit curve)
 *   06 K1_MULT         one k*G on secp256k1 (ECDH plain XY)
 *   07 SHA256D_80      SHA-256(SHA-256(80 bytes)): a block header's proof of work
 *   08 P256_VERIFY     verify the last P-256 signature -> 01
 *   09 NOOP
 */
public class Probe extends Applet {
    private static final byte[] K1_P = {(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFE,(byte)0xFF,(byte)0xFF,(byte)0xFC,(byte)0x2F};
    private static final byte[] K1_A = {(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00};
    private static final byte[] K1_B = {(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x07};
    private static final byte[] K1_G = {(byte)0x04,(byte)0x79,(byte)0xBE,(byte)0x66,(byte)0x7E,(byte)0xF9,(byte)0xDC,(byte)0xBB,(byte)0xAC,(byte)0x55,(byte)0xA0,(byte)0x62,(byte)0x95,(byte)0xCE,(byte)0x87,(byte)0x0B,(byte)0x07,(byte)0x02,(byte)0x9B,(byte)0xFC,(byte)0xDB,(byte)0x2D,(byte)0xCE,(byte)0x28,(byte)0xD9,(byte)0x59,(byte)0xF2,(byte)0x81,(byte)0x5B,(byte)0x16,(byte)0xF8,(byte)0x17,(byte)0x98,(byte)0x48,(byte)0x3A,(byte)0xDA,(byte)0x77,(byte)0x26,(byte)0xA3,(byte)0xC4,(byte)0x65,(byte)0x5D,(byte)0xA4,(byte)0xFB,(byte)0xFC,(byte)0x0E,(byte)0x11,(byte)0x08,(byte)0xA8,(byte)0xFD,(byte)0x17,(byte)0xB4,(byte)0x48,(byte)0xA6,(byte)0x85,(byte)0x54,(byte)0x19,(byte)0x9C,(byte)0x47,(byte)0xD0,(byte)0x8F,(byte)0xFB,(byte)0x10,(byte)0xD4,(byte)0xB8};
    private static final byte[] K1_N = {(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFE,(byte)0xBA,(byte)0xAE,(byte)0xDC,(byte)0xE6,(byte)0xAF,(byte)0x48,(byte)0xA0,(byte)0x3B,(byte)0xBF,(byte)0xD2,(byte)0x5E,(byte)0x8C,(byte)0xD0,(byte)0x36,(byte)0x41,(byte)0x41};
    private static final byte[] R1_P = {(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x01,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF};
    private static final byte[] R1_A = {(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x01,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFC};
    private static final byte[] R1_B = {(byte)0x5A,(byte)0xC6,(byte)0x35,(byte)0xD8,(byte)0xAA,(byte)0x3A,(byte)0x93,(byte)0xE7,(byte)0xB3,(byte)0xEB,(byte)0xBD,(byte)0x55,(byte)0x76,(byte)0x98,(byte)0x86,(byte)0xBC,(byte)0x65,(byte)0x1D,(byte)0x06,(byte)0xB0,(byte)0xCC,(byte)0x53,(byte)0xB0,(byte)0xF6,(byte)0x3B,(byte)0xCE,(byte)0x3C,(byte)0x3E,(byte)0x27,(byte)0xD2,(byte)0x60,(byte)0x4B};
    private static final byte[] R1_G = {(byte)0x04,(byte)0x6B,(byte)0x17,(byte)0xD1,(byte)0xF2,(byte)0xE1,(byte)0x2C,(byte)0x42,(byte)0x47,(byte)0xF8,(byte)0xBC,(byte)0xE6,(byte)0xE5,(byte)0x63,(byte)0xA4,(byte)0x40,(byte)0xF2,(byte)0x77,(byte)0x03,(byte)0x7D,(byte)0x81,(byte)0x2D,(byte)0xEB,(byte)0x33,(byte)0xA0,(byte)0xF4,(byte)0xA1,(byte)0x39,(byte)0x45,(byte)0xD8,(byte)0x98,(byte)0xC2,(byte)0x96,(byte)0x4F,(byte)0xE3,(byte)0x42,(byte)0xE2,(byte)0xFE,(byte)0x1A,(byte)0x7F,(byte)0x9B,(byte)0x8E,(byte)0xE7,(byte)0xEB,(byte)0x4A,(byte)0x7C,(byte)0x0F,(byte)0x9E,(byte)0x16,(byte)0x2B,(byte)0xCE,(byte)0x33,(byte)0x57,(byte)0x6B,(byte)0x31,(byte)0x5E,(byte)0xCE,(byte)0xCB,(byte)0xB6,(byte)0x40,(byte)0x68,(byte)0x37,(byte)0xBF,(byte)0x51,(byte)0xF5};
    private static final byte[] R1_N = {(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0x00,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xFF,(byte)0xBC,(byte)0xE6,(byte)0xFA,(byte)0xAD,(byte)0xA7,(byte)0x17,(byte)0x9E,(byte)0x84,(byte)0xF3,(byte)0xB9,(byte)0xCA,(byte)0xC2,(byte)0xFC,(byte)0x63,(byte)0x25,(byte)0x51};

    private final byte[] store = new byte[4096];
    private final KeyPair r1;
    private final KeyPair k1;
    private final Signature ecdsa;
    private final KeyAgreement mult;
    private final MessageDigest sha;
    private final byte[] ram;
    private final byte[] lastSig = new byte[80];
    private short lastSigLen;

    private Probe() {
        r1 = new KeyPair(KeyPair.ALG_EC_FP, KeyBuilder.LENGTH_EC_FP_256);
        curve((ECKey) r1.getPrivate(), R1_P, R1_A, R1_B, R1_G, R1_N);
        curve((ECKey) r1.getPublic(), R1_P, R1_A, R1_B, R1_G, R1_N);
        r1.genKeyPair();
        k1 = new KeyPair(KeyPair.ALG_EC_FP, KeyBuilder.LENGTH_EC_FP_256);
        curve((ECKey) k1.getPrivate(), K1_P, K1_A, K1_B, K1_G, K1_N);
        curve((ECKey) k1.getPublic(), K1_P, K1_A, K1_B, K1_G, K1_N);
        k1.genKeyPair();
        ecdsa = Signature.getInstance(Signature.ALG_ECDSA_SHA_256, false);
        mult = KeyAgreement.getInstance(KeyAgreement.ALG_EC_SVDP_DH_PLAIN_XY, false);
        sha = MessageDigest.getInstance(MessageDigest.ALG_SHA_256, false);
        ram = JCSystem.makeTransientByteArray((short) 128, JCSystem.CLEAR_ON_DESELECT);
    }

    private static void curve(ECKey k, byte[] p, byte[] a, byte[] b, byte[] g, byte[] n) {
        k.setFieldFP(p, (short) 0, (short) 32);
        k.setA(a, (short) 0, (short) 32);
        k.setB(b, (short) 0, (short) 32);
        k.setG(g, (short) 0, (short) 65);
        k.setR(n, (short) 0, (short) 32);
        k.setK((short) 1);
    }

    public static void install(byte[] b, short o, byte l) {
        new Probe().register(b, (short) (o + 1), b[o]);
    }

    public void process(APDU apdu) {
        if (selectingApplet()) return;
        byte[] buf = apdu.getBuffer();
        short p1 = (short) (buf[ISO7816.OFFSET_P1] & 0xFF);
        short p2 = (short) (buf[ISO7816.OFFSET_P2] & 0xFF);
        switch (buf[ISO7816.OFFSET_INS]) {
            case 0x01: {
                Util.setShort(buf, (short) 0, JCSystem.getMaxCommitCapacity());
                Util.setShort(buf, (short) 2, JCSystem.getUnusedCommitCapacity());
                Util.setShort(buf, (short) 4, JCSystem.getAvailableMemory(JCSystem.MEMORY_TYPE_PERSISTENT));
                apdu.setOutgoingAndSend((short) 0, (short) 6);
                return;
            }
            case 0x02: {
                short size = p2 == 0 ? 1 : p2;
                if ((short) (p1 * size) > (short) store.length) ISOException.throwIt(ISO7816.SW_WRONG_P1P2);
                try {
                    JCSystem.beginTransaction();
                    for (short i = 0; i < p1; i++) {
                        short at = (short) (i * size);
                        if (size == 1) store[at] = (byte) (store[at] + 1);
                        else Util.arrayCopy(ram, (short) 0, store, at, size);
                    }
                    JCSystem.commitTransaction();
                } catch (TransactionException e) {
                    if (JCSystem.getTransactionDepth() != 0) JCSystem.abortTransaction();
                    ISOException.throwIt((short) (0x6A00 | (e.getReason() & 0xFF)));
                }
                return;
            }
            case 0x03: r1.genKeyPair(); return;
            case 0x04: {
                ecdsa.init(r1.getPrivate(), Signature.MODE_SIGN);
                lastSigLen = ecdsa.sign(ram, (short) 0, (short) 32, lastSig, (short) 0);
                Util.arrayCopyNonAtomic(lastSig, (short) 0, buf, (short) 0, lastSigLen);
                apdu.setOutgoingAndSend((short) 0, lastSigLen);
                return;
            }
            case 0x05: {
                ecdsa.init(k1.getPrivate(), Signature.MODE_SIGN);
                short n = ecdsa.sign(ram, (short) 0, (short) 32, buf, (short) 0);
                apdu.setOutgoingAndSend((short) 0, n);
                return;
            }
            case 0x06: {
                mult.init(k1.getPrivate());
                short n = mult.generateSecret(K1_G, (short) 0, (short) 65, buf, (short) 0);
                apdu.setOutgoingAndSend((short) 0, n);
                return;
            }
            case 0x07: {
                sha.reset();
                sha.doFinal(store, (short) 0, (short) 80, ram, (short) 0);
                sha.doFinal(ram, (short) 0, (short) 32, buf, (short) 0);
                apdu.setOutgoingAndSend((short) 0, (short) 32);
                return;
            }
            case 0x08: {
                ecdsa.init(r1.getPublic(), Signature.MODE_VERIFY);
                buf[0] = ecdsa.verify(ram, (short) 0, (short) 32, lastSig, (short) 0, lastSigLen) ? (byte) 1 : (byte) 0;
                apdu.setOutgoingAndSend((short) 0, (short) 1);
                return;
            }
            case 0x09: return;
            default: ISOException.throwIt(ISO7816.SW_INS_NOT_SUPPORTED);
        }
    }
}
