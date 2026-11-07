package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.CardTestSupport.*;
import static org.junit.jupiter.api.Assertions.*;

import java.io.ByteArrayOutputStream;
import java.math.BigInteger;
import java.nio.charset.StandardCharsets;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.interfaces.ECPublicKey;
import java.security.spec.ECGenParameterSpec;
import javax.smartcardio.ResponseAPDU;
import org.junit.jupiter.api.Test;

/**
 * RELEASE (docs/OFFLINE-PROTOCOL.md § 6): the registry's signature undoes what the card did for a
 * transaction that will never be accepted, and nothing else.
 */
class ReleaseTest {
    static KeyPair registry() throws Exception {
        KeyPairGenerator g = KeyPairGenerator.getInstance("EC");
        g.initialize(new ECGenParameterSpec("secp256r1"));
        return g.generateKeyPair();
    }

    static byte[] point(KeyPair kp) {
        java.security.spec.ECPoint w = ((ECPublicKey) kp.getPublic()).getW();
        byte[] out = new byte[65];
        out[0] = 4;
        byte[] x = Bip340.bytes32(w.getAffineX()), y = Bip340.bytes32(w.getAffineY());
        System.arraycopy(x, 0, out, 1, 32);
        System.arraycopy(y, 0, out, 33, 32);
        return out;
    }

    /** RELEASE's data: txid || DER signature over "fedimintcard/rel1" || slot 0 || slot || txid. */
    static byte[] release(KeyPair kp, byte[] slot0, int slot, byte[] txid) throws Exception {
        java.security.Signature s = java.security.Signature.getInstance("SHA256withECDSA");
        s.initSign(kp.getPrivate());
        s.update("fedimintcard/rel1".getBytes(StandardCharsets.US_ASCII));
        s.update(slot0);
        s.update((byte) slot);
        s.update(txid);
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        out.write(txid);
        out.write(s.sign());
        return out.toByteArray();
    }

    /** A card with the registry key, slot 0 identity, slot 1 a note, slot 2 keyed; PIN verified. */
    static CardTestSupport card(KeyPair kp) {
        CardTestSupport c = new CardTestSupport();
        c.genKey(0);
        c.genKey(1);
        c.genKey(2);
        assertSw(0x9000, c.tx(SET_NOTE, 1, noteData(2048)));
        assertSw(0x9000, c.tx(SET_REGISTRY_KEY, 0, point(kp)));
        c.setPin(DEFAULT_PIN);
        c.verifyPin(DEFAULT_PIN);
        return c;
    }

    static byte[] change(byte[] txid) {
        byte[] d = new byte[74];
        System.arraycopy(OfflineTest.longBytes(1024), 0, d, 0, 8);
        System.arraycopy(txid, 0, d, 8, 32);
        return d;
    }

    @Test
    void aReleasedInputIsANoteAgainAndSignsAnotherTransaction() throws Exception {
        KeyPair kp = registry();
        CardTestSupport c = card(kp);
        byte[] t1 = fill(32, 0x31), t2 = fill(32, 0x32);
        assertSw(0x9000, c.tx(SIGN, 1, t1));
        assertSw(0x6985, c.tx(SIGN, 1, t2), "spent on t1");
        ResponseAPDU r = c.tx(RELEASE, 1, release(kp, c.getPub(0), 1, t1));
        assertSw(0x9000, r);
        assertEquals(ST_NOTE, r.getData()[0]);
        byte[] sig = c.tx(SIGN, 1, t2).getData();
        assertTrue(Bip340.verify(c.getPub(1), t2, sig), "the note signs its new transaction");
        // the old release cannot undo the new spend
        assertSw(0x6985, c.tx(RELEASE, 1, release(kp, c.getPub(0), 1, t1)));
    }

    @Test
    void aReleasedChangeSlotIsKeyedAgain() throws Exception {
        KeyPair kp = registry();
        CardTestSupport c = card(kp);
        byte[] t = fill(32, 0x44);
        assertSw(0x9000, c.tx(SET_CHANGE, 2, change(t)));
        assertSw(0x6985, c.tx(CLA, GEN_KEY, 2, 1, null), "change owed: never re-keyed");
        ResponseAPDU r = c.tx(RELEASE, 2, release(kp, c.getPub(0), 2, t));
        assertSw(0x9000, r);
        assertEquals(ST_KEYED, r.getData()[0]);
        byte[] slot = c.tx(GET_SLOT, 2, null).getData();
        assertArrayEquals(new byte[66], java.util.Arrays.copyOfRange(slot, 121, 187), "the opening is gone");
        assertSw(0x9000, c.tx(SET_NOTE, 2, noteData(4096)), "the slot takes a new note");
    }

    @Test
    void onlyTheRegistrysSignatureOverThisCardSlotAndTransaction() throws Exception {
        KeyPair kp = registry(), other = registry();
        CardTestSupport c = card(kp);
        byte[] t = fill(32, 0x55);
        assertSw(0x9000, c.tx(SIGN, 1, t));
        assertSw(0x6982, c.tx(RELEASE, 1, release(other, c.getPub(0), 1, t)), "another key");
        assertSw(0x6982, c.tx(RELEASE, 1, release(kp, fill(32, 7), 1, t)), "another card");
        assertSw(0x6982, c.tx(RELEASE, 1, release(kp, c.getPub(0), 2, t)), "signed for another slot");
        assertSw(0x6985, c.tx(RELEASE, 2, release(kp, c.getPub(0), 2, t)), "slot 2 never spent on t");
        assertSw(0x6985, c.tx(RELEASE, 1, release(kp, c.getPub(0), 1, fill(32, 0x56))), "another transaction");
        byte[] bad = release(kp, c.getPub(0), 1, t);
        bad[bad.length - 1] ^= 1;
        assertSw(0x6982, c.tx(RELEASE, 1, bad));
        assertEquals(ST_SPENT, c.slotState(1), "nothing was released");
    }

    @Test
    void theRegistryKeyIsSetBeforeThePinAndWithoutItNothingIsReleased() throws Exception {
        KeyPair kp = registry();
        CardTestSupport c = new CardTestSupport();
        c.genKey(0);
        c.genKey(1);
        assertSw(0x9000, c.tx(SET_NOTE, 1, noteData(2048)));
        assertEquals(0, c.tx(GET_STATE, 0, null).getData()[58]);
        c.setPin(DEFAULT_PIN);
        c.verifyPin(DEFAULT_PIN);
        assertSw(0x6985, c.tx(SET_REGISTRY_KEY, 0, point(kp)), "only before the PIN");
        byte[] t = fill(32, 0x66);
        assertSw(0x9000, c.tx(SIGN, 1, t));
        assertSw(0x6985, c.tx(RELEASE, 1, release(kp, c.getPub(0), 1, t)), "no registry key");
        byte[] st = c.tx(GET_STATE, 0, null).getData();
        assertEquals(59, st.length);
    }

    @Test
    void aMalformedKeyOrReleaseIsRefused() throws Exception {
        CardTestSupport c = new CardTestSupport();
        byte[] notAPoint = new byte[65];
        notAPoint[0] = 4;
        assertSw(0x6700, c.tx(SET_REGISTRY_KEY, 0, new byte[64]));
        byte[] compressed = new byte[65];
        compressed[0] = 2;
        assertSw(0x6A80, c.tx(SET_REGISTRY_KEY, 0, compressed));
        KeyPair kp = registry();
        assertSw(0x9000, c.tx(SET_REGISTRY_KEY, 0, point(kp)));
        assertEquals(1, c.tx(GET_STATE, 0, null).getData()[58]);
        assertSw(0x6700, c.tx(RELEASE, 0, new byte[33]));
    }
}
