package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.CardTestSupport.*;
import static org.junit.jupiter.api.Assertions.*;

import java.math.BigInteger;
import java.util.Arrays;
import java.util.Map;
import org.junit.jupiter.api.Test;

/**
 * SET_NOTE (DESIGN § 2 and § 6): the note (56) || the registry's DER signature (8 to 72) over "fedimintcard/note1" ||
 * the slot's x-only key || the note. Checked under the registry key; a bad one is 6982 and writes nothing.
 */
class NoteSignatureTest {
    static final byte[] PIN = { '1', '2', '3', '4' };
    static final BigInteger N = new BigInteger("FFFFFFFF00000000FFFFFFFFFFFFFFFFBCE6FAADA7179E84F3B9CAC2FC632551", 16);

    static CardTestSupport keyed(int... slots) {
        CardTestSupport c = new CardTestSupport();
        for (int s : slots) assertSw(0x9000, c.tx(GEN_KEY, s, null));
        c.setPin(PIN);
        return c;
    }

    static byte[] withSig(byte[] note, byte[] sig) {
        byte[] out = Arrays.copyOf(note, 56 + sig.length);
        System.arraycopy(sig, 0, out, 56, sig.length);
        return out;
    }

    /** DER's two integers. */
    static BigInteger[] rs(byte[] der) {
        int rl = der[3];
        byte[] r = Arrays.copyOfRange(der, 4, 4 + rl);
        int sl = der[5 + rl];
        byte[] s = Arrays.copyOfRange(der, 6 + rl, 6 + rl + sl);
        return new BigInteger[] { new BigInteger(1, r), new BigInteger(1, s) };
    }

    static byte[] der(BigInteger r, BigInteger s) {
        byte[] rb = r.toByteArray(), sb = s.toByteArray();
        byte[] out = new byte[6 + rb.length + sb.length];
        out[0] = 0x30;
        out[1] = (byte) (4 + rb.length + sb.length);
        out[2] = 0x02;
        out[3] = (byte) rb.length;
        System.arraycopy(rb, 0, out, 4, rb.length);
        out[4 + rb.length] = 0x02;
        out[5 + rb.length] = (byte) sb.length;
        System.arraycopy(sb, 0, out, 6 + rb.length, sb.length);
        return out;
    }

    @Test
    void aRegistrySignedNoteLoadsWithoutThePin() {
        CardTestSupport c = keyed(0);
        c.select();   // a new selection: no PIN given
        byte[] d = c.note(0, 1000);
        assertSw(0x9000, c.tx(SET_NOTE, 0, d));
        assertEquals(ST_NOTE, c.slotState(0));
        assertArrayEquals(Arrays.copyOf(d, 56), Arrays.copyOfRange(c.tx(GET_SLOT, 0, null).getData(), 33, 89));
    }

    @Test
    void aBadSignatureIs6982AndWritesNothing() throws Exception {
        CardTestSupport c = keyed(0, 1);
        byte[] x0 = c.slotX(0), x1 = c.slotX(1), note = noteData(1000);
        byte[] good = Registry.signed(x0, note);
        byte[] flipped = good.clone();
        flipped[good.length - 1] ^= 1;
        byte[] otherAmount = good.clone();
        otherAmount[7] ^= 1;
        byte[] otherFed = good.clone();
        otherFed[55] ^= 1;
        byte[] junk = withSig(note, CardTestSupport.fill(72, 0x30));
        Map<String, Object> before = c.persistent();
        for (byte[] bad : new byte[][] {
            Registry.signed(Registry.privateKey(BigInteger.TWO), x0, note),   // not the registry
            Registry.signed(x1, note),                                         // the registry, for slot 1's key
            flipped, otherAmount, otherFed, junk }) {
            assertSw(0x6982, c.tx(SET_NOTE, 0, bad));
            assertEquals(before, c.persistent(), "nothing written");
        }
        assertSw(0x9000, c.tx(SET_NOTE, 0, good));
    }

    /**
     * A verify that throws is a refusal too. jCardSim answers false to any signature bytes (its decoder catches its own
     * errors), so the applet's verifier is swapped for one that throws, as a chip's may on DER it cannot parse.
     */
    @Test
    void aVerifyThatThrowsIs6982AndWritesNothing() throws Exception {
        CardTestSupport c = keyed(0);
        byte[] good = c.note(0, 1000);
        Object real = c.field("ecdsa");
        c.setField("ecdsa", new com.licel.jcardsim.crypto.AsymmetricSignatureImpl(javacard.security.Signature.ALG_ECDSA_SHA_256) {
            @Override
            public boolean verify(byte[] in, short inOff, short inLen, byte[] sig, short sigOff, short sigLen) {
                javacard.security.CryptoException.throwIt(javacard.security.CryptoException.ILLEGAL_VALUE);
                return true;
            }
        });
        Map<String, Object> before = c.persistent();
        assertSw(0x6982, c.tx(SET_NOTE, 0, good));
        assertEquals(before, c.persistent(), "nothing written");
        assertEquals(ST_KEYED, c.slotState(0));
        c.setField("ecdsa", real);
        assertSw(0x9000, c.tx(SET_NOTE, 0, good));
    }

    @Test
    void withoutARegistryKeyTheCardTakesNoNote() throws Exception {
        CardTestSupport c = new CardTestSupport();
        assertSw(0x9000, c.tx(GEN_KEY, 0, null));
        assertFalse((Boolean) c.field("registryKeySet"));
        Map<String, Object> before = c.persistent();
        assertSw(0x6985, c.tx(SET_NOTE, 0, Registry.signed(c.slotX(0), noteData(1000))));
        assertEquals(before, c.persistent());
    }

    @Test
    void aReKeyedSlotRefusesTheNoteForItsOldKey() {
        CardTestSupport c = keyed(1);
        byte[] old = c.note(1, 1000);
        c.verifyPin(PIN);
        assertSw(0x9000, c.tx(CLA, GEN_KEY, 1, 1, null));
        assertSw(0x6982, c.tx(SET_NOTE, 1, old));
        assertEquals(ST_KEYED, c.slotState(1));
        assertSw(0x9000, c.tx(SET_NOTE, 1, c.note(1, 1000)));
    }

    @Test
    void lengths() {
        CardTestSupport c = keyed(0);
        byte[] good = c.note(0, 1000);
        assertSw(0x6700, c.tx(SET_NOTE, 0, noteData(1000)), "no unsigned form");
        assertSw(0x6700, c.tx(SET_NOTE, 0, withSig(noteData(1000), new byte[7])));
        byte[] shortest = { 0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01 };   // r = s = 1: 8 bytes reach the verify
        assertSw(0x6982, c.tx(SET_NOTE, 0, withSig(noteData(1000), shortest)));
        assertSw(0x6700, c.tx(SET_NOTE, 0, withSig(noteData(1000), new byte[73])));
        assertSw(0x6700, c.tx(SET_NOTE, 0, Arrays.copyOf(good, 56 + 73)), "a byte past the signature");
        assertSw(0x6700, c.tx(SET_NOTE, 0, new byte[55]));
        assertEquals(ST_KEYED, c.slotState(0));
        assertSw(0x9000, c.tx(SET_NOTE, 0, good));
    }

    /** The registry's signatures are WebCrypto's: s is not normalised, and DER runs 70 to 72 bytes. */
    @Test
    void bothHalvesOfSAndTheLongestSignatureAreTaken() {
        CardTestSupport c = keyed(0, 1, 2);
        byte[] note = noteData(1000);
        BigInteger[] rs = rs(Arrays.copyOfRange(c.note(0, 1000), 56, 56 + 72));
        byte[] low = der(rs[0], rs[1].min(N.subtract(rs[1])));
        byte[] high = der(rs[0], rs[1].max(N.subtract(rs[1])));
        assertSw(0x9000, c.tx(SET_NOTE, 0, withSig(note, low)), "low s");
        byte[] hs = Registry.signed(c.slotX(1), note);
        BigInteger[] rs1 = rs(Arrays.copyOfRange(hs, 56, hs.length));
        assertSw(0x9000, c.tx(SET_NOTE, 1, withSig(note, der(rs1[0], rs1[1].max(N.subtract(rs1[1]))))), "high s");
        assertTrue(high.length >= 70);
        byte[] longest;
        do longest = Registry.signed(c.slotX(2), note); while (longest.length != 56 + 72);
        assertSw(0x9000, c.tx(SET_NOTE, 2, longest), "a 72-byte signature, read whole");
    }
}
