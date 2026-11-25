package io.github.bobodread876.fedimintcard.applet.proof;

import static io.github.bobodread876.fedimintcard.applet.proof.SimCard.*;
import static org.junit.jupiter.api.Assertions.*;

import java.util.Arrays;
import javacard.framework.JCSystem;
import org.junit.jupiter.api.Test;

/**
 * select() and deselect() each begin a selection with nothing in it: the working arrays x and tmp zeroed, nothing held
 * (the PIN given by SET_PIN, the anchor, the stretch of the wait), and a new PIN a torn PIN write left behind wiped. They
 * do it themselves, beside the runtime's CLEAR_ON_DESELECT clearing, for a chip that keeps a same-package applet's
 * transient memory across a SELECT.
 */
class SelectClearsTest {

    static void dirty(SimCard c) throws Exception {
        // REFLECTION: stands for what the last command, or a torn one, left
        for (String f : new String[] { "x", "tmp", "held", "newPin" }) Arrays.fill((byte[]) c.field(f), (byte) 0x5A);
    }

    static void clean(SimCard c, String when) throws Exception {
        for (String f : new String[] { "x", "tmp", "held", "newPin" })
            for (byte b : (byte[]) c.field(f)) assertEquals(0, b, f + " after " + when);
    }

    @Test
    void selectAndDeselectZeroTheWorkingArraysAndWhatIsHeld() throws Exception {
        SimCard c = new SimCard();
        assertEquals(JCSystem.CLEAR_ON_DESELECT, c.transientClass("x"));
        assertEquals(JCSystem.CLEAR_ON_DESELECT, c.transientClass("tmp"));
        assertEquals(JCSystem.CLEAR_ON_DESELECT, c.transientClass("held"));
        dirty(c);
        c.applet().deselect();
        clean(c, "deselect()");
        dirty(c);
        c.applet().select();
        clean(c, "select()");
    }

    @Test
    void aNewSelectionHasNoPinGivenAndNoAnchor() throws Exception {
        SimCard c = new SimCard();
        assertEquals(0x9000, c.setPin());
        assertEquals(0x6985, c.sw(SIGN, 0, new byte[32]), "SET_PIN's selection has the PIN: past the gate, an empty slot");
        ((byte[]) c.field("held"))[1] = 1;   // REFLECTION: an anchor in this selection (the clock's slice sets it)
        assertEquals(1, c.tx(GET_STATE, 0, null).getData()[63]);
        assertEquals(0x9000, c.select().getSW());
        assertEquals(0x6A94, c.sw(SIGN, 0, new byte[32]), "a new selection: no PIN given");
        assertEquals(0, c.tx(GET_STATE, 0, null).getData()[63], "and no anchor");
    }
}
