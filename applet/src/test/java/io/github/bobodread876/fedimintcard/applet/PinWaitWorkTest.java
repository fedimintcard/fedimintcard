package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.CardTestSupport.*;
import static io.github.bobodread876.fedimintcard.applet.ChipEnginesTest.get;
import static io.github.bobodread876.fedimintcard.applet.ChipEnginesTest.set;
import static org.junit.jupiter.api.Assertions.*;

import javacard.security.ECPrivateKey;
import javacard.security.ECPublicKey;
import javacard.security.KeyAgreement;
import javacard.security.KeyPair;
import javacard.security.MessageDigest;
import org.junit.jupiter.api.Test;

/**
 * Each asking of the PIN's wait does its work: on a signer without the nonce key in RAM (the simulator's), exactly
 * WORK_HASHES SHA-256 rounds over 32 bytes; on one with it, one signature with the work keys and no rounds. The end of a
 * stretch does none. An asking is counted only once its work is done. The work is seen through the applet's own digest
 * and the signer's engines, put behind counting ones by reflection; the applet is not changed for the test.
 */
class PinWaitWorkTest {
    static final int KEEP_HOLDING = 0x6A98;

    /** The applet's SHA-256, counted: every doFinal and whether each took 32 bytes; throws at the doFinal `throwAt`. */
    static final class CountedDigest extends MessageDigest {
        final MessageDigest real = MessageDigest.getInstance(MessageDigest.ALG_SHA_256, false);
        int finals, notThirtyTwo, throwAt = -1;
        @Override public byte getAlgorithm() { return real.getAlgorithm(); }
        @Override public byte getLength() { return real.getLength(); }
        @Override public short doFinal(byte[] in, short off, short len, byte[] out, short outOff) {
            if (finals == throwAt) throw new ArrayIndexOutOfBoundsException("a fault inside the work");
            finals++;
            if (len != 32) notThirtyTwo++;
            return real.doFinal(in, off, len, out, outOff);
        }
        @Override public void update(byte[] in, short off, short len) { real.update(in, off, len); }
        @Override public void reset() { real.reset(); }
    }

    /** A key agreement in front of the simulator's that counts its answers. */
    static final class CountedAgreement extends KeyAgreement {
        final KeyAgreement real;
        int calls;
        CountedAgreement(KeyAgreement real) { this.real = real; }
        @Override public void init(javacard.security.PrivateKey k) { real.init(k); }
        @Override public byte getAlgorithm() { return real.getAlgorithm(); }
        @Override public short generateSecret(byte[] pub, short off, short len, byte[] out, short outOff) {
            calls++;
            return real.generateSecret(pub, off, len, out, outOff);
        }
    }

    /** A card with a PIN, in a new selection, owing `owed` askings after one wrong try; its digest counted. */
    static CardTestSupport owing(int owed, CountedDigest d) throws Exception {
        CardTestSupport c = new CardTestSupport();
        c.setPin(DEFAULT_PIN);
        c.select();
        byte[] m = (byte[]) c.field("pinMarks");
        m[0] = 1;
        m[1] = (byte) (owed >> 8);
        m[2] = (byte) owed;
        c.setField("sha", d);
        return c;
    }

    @Test
    void anAskingDoesItsRoundsAndTheStretchEndDoesNone() throws Exception {
        assertEquals(512, FedimintCardApplet.WORK_HASHES,
            "the rounds of an asking: C1 sets this from the chip whose rounds run faster, and no lower");
        CountedDigest d = new CountedDigest();
        CardTestSupport c = owing(2, d);
        for (int i = 1; i <= 2; i++) {
            d.finals = 0;
            assertSw(KEEP_HOLDING, c.verifyPinRaw(DEFAULT_PIN), "asking " + i);
            assertEquals(FedimintCardApplet.WORK_HASHES, d.finals, "asking " + i + " did every round");
        }
        assertEquals(0, d.notThirtyTwo, "each round over 32 bytes");
        d.finals = 0;
        assertSw(0x9000, c.verifyPinRaw(DEFAULT_PIN), "the stretch ends, nothing more is owed, and the right PIN opens");
        // The sealed try hashes the seal's two blocks (the tag's, then the keystream's), and nothing more.
        assertEquals(2, d.finals, "the stretch's end does no rounds, and the try only the seal's two blocks");
    }

    @Test
    void onASignerWithTheNonceKeyInRamAnAskingIsOneSignature() throws Exception {
        CountedDigest d = new CountedDigest();
        CardTestSupport c = owing(1, d);
        SchnorrHW s = (SchnorrHW) c.field("schnorr");
        set(s, SchnorrHW.class, "keyInRam", true);
        CountedAgreement ecdh = new CountedAgreement((KeyAgreement) get(s, SchnorrHW.class, "ecdh"));
        CountedAgreement gm = new CountedAgreement((KeyAgreement) get(s, SchnorrHW.class, "gm"));
        set(s, SchnorrHW.class, "ecdh", ecdh);
        set(s, SchnorrHW.class, "gm", gm);
        KeyPair work = (KeyPair) c.field("workKeys");
        s.sign((ECPrivateKey) work.getPrivate(), (ECPublicKey) work.getPublic(), fill(32, 3), (short) 0, new byte[64], (short) 0);
        int perSig = ecdh.calls + gm.calls;
        assertTrue(perSig > 0, "a signature asks the engines");
        ecdh.calls = 0;
        gm.calls = 0;
        assertSw(KEEP_HOLDING, c.verifyPinRaw(DEFAULT_PIN));
        assertEquals(perSig, ecdh.calls + gm.calls, "the asking made one signature's engine calls");
        assertEquals(0, d.finals, "and no rounds");
        assertSw(0x9000, c.verifyPinRaw(DEFAULT_PIN));
    }

    @Test
    void anAskingWhoseWorkEndsEarlyIsNotCounted() throws Exception {
        CountedDigest d = new CountedDigest();
        CardTestSupport c = owing(1, d);
        d.throwAt = 3;
        assertSw(0x6F00, c.verifyPinRaw(DEFAULT_PIN), "a fault inside the work");
        d.throwAt = -1;
        d.finals = 0;
        assertSw(KEEP_HOLDING, c.verifyPinRaw(DEFAULT_PIN), "the asking after it is still the stretch's first");
        assertEquals(FedimintCardApplet.WORK_HASHES, d.finals, "and does all its rounds");
        assertSw(0x9000, c.verifyPinRaw(DEFAULT_PIN), "then the stretch ends and the right PIN opens");
        assertEquals(0, ((byte[]) c.field("pinMarks"))[2]);
    }
}
