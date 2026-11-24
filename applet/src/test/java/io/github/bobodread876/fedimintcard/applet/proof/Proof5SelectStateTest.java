package io.github.bobodread876.fedimintcard.applet.proof;

import static io.github.bobodread876.fedimintcard.applet.proof.SimCard.*;
import static org.junit.jupiter.api.Assertions.*;

import java.util.Arrays;
import javacard.framework.JCSystem;
import org.junit.jupiter.api.Test;

/**
 * Review finding 5: select() and deselect() reset the PIN and nothing else; the applet's scratch arrays x and tmp
 * keep what they held, and only the runtime's CLEAR_ON_DESELECT clearing (where it happens) empties them. The fix
 * (zero x and tmp in both) must make this test fail.
 */
class Proof5SelectStateTest {

    @Test
    void selectAndDeselectLeaveXAndTmp() throws Exception {
        SimCard c = new SimCard();
        assertEquals(JCSystem.CLEAR_ON_DESELECT, c.transientClass("x"));
        assertEquals(JCSystem.CLEAR_ON_DESELECT, c.transientClass("tmp"));
        byte[] x = (byte[]) c.field("x");
        byte[] tmp = (byte[]) c.field("tmp");
        Arrays.fill(x, (byte) 0x5A);    // REFLECTION: stands for what the last command left
        Arrays.fill(tmp, (byte) 0x5A);

        // The applet's own methods, called directly: what select() and deselect() clear, apart from the runtime.
        c.applet().deselect();
        c.applet().select();
        for (byte b : x) assertEquals((byte) 0x5A, b, "x is left by deselect() and select()");
        for (byte b : tmp) assertEquals((byte) 0x5A, b, "tmp is left by deselect() and select()");
    }
}
