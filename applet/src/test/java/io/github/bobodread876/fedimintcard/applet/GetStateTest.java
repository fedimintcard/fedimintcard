package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.CardTestSupport.*;
import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;

/** GET_STATE's 65 bytes: today's 59, the PIN state, the wrong tries, the askings owed (2), anchored, the signer. */
class GetStateTest {

    static byte[] state(CardTestSupport c) {
        byte[] st = c.tx(GET_STATE, 0, null).getData();
        assertEquals(65, st.length);
        return st;
    }

    @Test
    void thePinTheWaitTheAnchorAndTheSigner() throws Exception {
        CardTestSupport c = new CardTestSupport();
        byte[] st = state(c);
        assertEquals(0, st[59], "no PIN");
        assertEquals(0, st[63], "not anchored");
        assertEquals(0, st[64], "the simulator: no chip product, no nonce key in RAM");
        c.setPin(DEFAULT_PIN);
        c.select();
        assertSw(0x63C1, c.verifyPinRaw(fill(4, '9')));
        assertSw(0x6A98, c.verifyPinRaw(DEFAULT_PIN), "one asking owed");
        st = state(c);
        assertEquals(1, st[59]);
        assertEquals(1, st[60], "one wrong try");
        assertEquals(0, st[61]);
        assertEquals(1, st[62], "one asking owed (the stretch has not ended)");
        // REFLECTION: the anchor (the clock's slice sets it) and the signer's two bits
        ((byte[]) c.field("held"))[FedimintCardApplet.H_ANCHORED] = 1;
        SchnorrHW s = (SchnorrHW) c.field("schnorr");
        s.chipMul = true;
        assertEquals(1, state(c)[63]);
        assertEquals(1, state(c)[64]);
        ChipEnginesTest.set(s, SchnorrHW.class, "keyInRam", true);
        assertEquals(3, state(c)[64]);
        s.chipMul = false;
        assertEquals(2, state(c)[64]);
    }
}
