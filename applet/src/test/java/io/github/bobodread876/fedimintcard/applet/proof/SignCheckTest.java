package io.github.bobodread876.fedimintcard.applet.proof;

import static io.github.bobodread876.fedimintcard.applet.proof.SimCard.*;
import static org.junit.jupiter.api.Assertions.*;

import io.github.bobodread876.fedimintcard.applet.Bip340;
import java.math.BigInteger;
import java.util.Arrays;
import javacard.security.ECPrivateKey;
import javax.smartcardio.ResponseAPDU;
import org.junit.jupiter.api.Test;

/**
 * SIGN checks its signature on the card before anything is written: one that fails the check answers 6F00, and the
 * slot stays a note with nothing stored, so the next SIGN signs afresh. Only a checked signature moves the slot to
 * SPENT.
 */
class SignCheckTest {

    @Test
    void aSignatureThatFailsTheCheckLeavesTheSlotANote() throws Exception {
        SimCard c = new SimCard();
        byte[] pubX = c.tx(GEN_KEY, 0, null).getData();
        assertEquals(0x9000, c.sw(SET_NOTE, 0, noteData(1_000_000L)));
        assertEquals(0x9000, c.sw(SET_PIN, 0, PIN));
        assertEquals(0x9000, c.sw(VERIFY_PIN, 0, PIN));
        byte[] stateBefore = c.state();
        byte[] spentSig = ((byte[]) c.field("spentSig")).clone();
        byte[] spentHash = ((byte[]) c.field("spentHash")).clone();

        // REFLECTION: stands for a fault during the signature. The slot's scalar is changed to d+1 for this one SIGN
        // and put back after it, as a glitch in the e·d product or the key read would be.
        ECPrivateKey k = ((ECPrivateKey[]) c.field("privs"))[0];
        byte[] buf = new byte[64];
        int n = k.getS(buf, (short) 0);
        BigInteger d = new BigInteger(1, Arrays.copyOf(buf, n));
        k.setS(Bip340.bytes32(d.add(BigInteger.ONE).mod(Bip340.N)), (short) 0, (short) 32);

        byte[] msg = SimCard.sha256(new byte[] { 1 });
        ResponseAPDU r = c.tx(SIGN, 0, msg);
        k.setS(Bip340.bytes32(d), (short) 0, (short) 32);   // the fault is over

        assertEquals(0x6F00, r.getSW(), "a signature that fails the check is not answered");
        assertEquals(0, r.getData().length, "and nothing of it leaves the card");
        assertEquals(ST_NOTE, c.slotState(0), "the slot is still a note");
        assertArrayEquals(spentSig, (byte[]) c.field("spentSig"), "no signature is stored");
        assertArrayEquals(spentHash, (byte[]) c.field("spentHash"), "no spent hash is stored");
        assertArrayEquals(stateBefore, c.state(), "the day's count and the clock are as they were");
        assertEquals(0x6985, c.tx(CLA, GEN_KEY, 0, 1, null).getSW(), "the note's key cannot be re-keyed away");
        assertArrayEquals(pubX, c.tx(0x11, 0, null).getData(), "and it is still the slot's key");

        // The next SIGN, of the same hash or another, signs afresh, and that signature is checked and kept.
        byte[] msg2 = SimCard.sha256(new byte[] { 2 });
        ResponseAPDU good = c.tx(SIGN, 0, msg2);
        assertEquals(0x9000, good.getSW());
        assertTrue(Bip340.verify(pubX, msg2, good.getData()), "the signature verifies under the slot's key");
        assertEquals(ST_SPENT, c.slotState(0));
        assertArrayEquals(good.getData(), Arrays.copyOfRange((byte[]) c.field("spentSig"), 0, 64), "and it is the one stored");
        ResponseAPDU again = c.tx(SIGN, 0, msg2);
        assertEquals(0x9000, again.getSW());
        assertArrayEquals(good.getData(), again.getData(), "a retry answers the stored signature");
        assertEquals(0x6985, c.sw(SIGN, 0, msg), "and no other transaction is signed");
    }

    @Test
    void aFaultOnEveryTryNeverSpendsTheSlot() throws Exception {
        SimCard c = new SimCard();
        c.tx(GEN_KEY, 0, null);
        assertEquals(0x9000, c.sw(SET_NOTE, 0, noteData(1_000_000L)));
        assertEquals(0x9000, c.sw(SET_PIN, 0, PIN));
        assertEquals(0x9000, c.sw(VERIFY_PIN, 0, PIN));
        ECPrivateKey k = ((ECPrivateKey[]) c.field("privs"))[0];
        byte[] buf = new byte[64];
        int n = k.getS(buf, (short) 0);
        BigInteger d = new BigInteger(1, Arrays.copyOf(buf, n));
        // REFLECTION: a fault that lasts, as a key read that gives the wrong scalar every time.
        k.setS(Bip340.bytes32(d.add(BigInteger.valueOf(7)).mod(Bip340.N)), (short) 0, (short) 32);
        for (int i = 0; i < 3; i++) {
            assertEquals(0x6F00, c.sw(SIGN, 0, SimCard.sha256(new byte[] { (byte) i })));
            assertEquals(ST_NOTE, c.slotState(0));
        }
        assertEquals(0, c.spentToday(), "nothing was counted against the day");
    }
}
