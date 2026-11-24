package io.github.bobodread876.fedimintcard.applet;

import static io.github.bobodread876.fedimintcard.applet.PinWaitTest.*;
import static io.github.bobodread876.fedimintcard.applet.TearHarnessTest.*;
import static org.junit.jupiter.api.Assertions.*;

import java.util.ArrayList;
import java.util.List;
import org.junit.jupiter.api.Test;

/**
 * SET_PIN torn after each of its writes in turn. Torn before the state byte (its commit point), the card reads as having
 * no PIN and takes SET_PIN again; torn after it, the PIN is set with its work keys and opens the card. Either way the new
 * PIN's bytes are gone at the next selection, and nothing is held.
 */
class SetPinTearTest {

    static Card blank() throws Exception {
        Card c = new Card();
        assertEquals(0x9000, c.tx(0x34, 0, TEST_REGISTRY_KEY).getSW());
        return c;
    }

    static boolean zero(byte[] a) {
        for (byte b : a) if (b != 0) return false;
        return true;
    }

    @Test
    void aSetPinTornAnywhereIsSetWithItsKeysOrNotSetAtAll() throws Exception {
        Card c = blank();
        TearHook.count = 0;
        TearHook.trace = new ArrayList<>();
        assertEquals(0x9000, c.tx(SET_PIN, 0, PIN).getSW());
        List<String> trace = TearHook.trace;
        TearHook.trace = null;
        int total = TearHook.count;
        String all = String.join("\n", trace);
        int commit = -1, keys = -1, update = -1;
        for (int i = 0; i < trace.size(); i++) {
            if (trace.get(i).contains("pinState stored")) commit = i + 1;
            if (trace.get(i).contains("genKeyPair")) keys = i + 1;
            if (trace.get(i).contains("pin.update")) update = i + 1;
        }
        assertTrue(commit > 0 && keys > 0 && update > 0, all);
        assertTrue(keys < update && update < commit, "the work keys, the PIN, then the state byte:\n" + all);
        System.out.println("SET_PIN tear sweep: " + total + " writes, each torn");
        List<String> wrong = new ArrayList<>();
        int unset = 0, set = 0;
        for (int k = 1; k <= total; k++) {
            Card t = blank();
            TearHook.count = 0;
            TearHook.target = k;
            int r = t.tx(SET_PIN, 0, PIN).getSW();
            String at = TearHook.tornAt;
            TearHook.target = 0;
            if (at == null) { wrong.add("write " + k + " was not reached"); continue; }
            if (r == 0x9000) wrong.add("write " + k + ": a torn SET_PIN answered 9000");
            t.powerCycle();
            if (!zero((byte[]) t.field("newPin"))) wrong.add("write " + k + " (" + at + "): the new PIN outlived the selection");
            if (!zero((byte[]) t.field("held"))) wrong.add("write " + k + ": something held after a power cycle");
            if (tries(t) == 0) wrong.add("write " + k + ": the chip's counter ran out");
            if (pinState(t) == 0) {
                unset++;
                if (k >= commit) wrong.add("write " + k + ": torn after the commit point, and not set");
                if (t.tx(SET_PIN, 0, PIN).getSW() != 0x9000) wrong.add("write " + k + ": SET_PIN does not run again");
                t.select();
            } else {
                set++;
                if (k < commit) wrong.add("write " + k + ": set before its commit point");
                if (t.field("workKeys") == null) wrong.add("write " + k + ": a PIN without its work keys");
            }
            int[] m = marks(t);
            if (m[0] != 0 || m[1] != 0) wrong.add("write " + k + ": marks " + m[0] + "/" + m[1]);
            int[] a = asked(t, PIN);
            if (a[0] != 0 || a[1] != 0x9000) wrong.add("write " + k + ": the PIN then answered " + Integer.toHexString(a[1]) + " after " + a[0]);
        }
        assertEquals(List.of(), wrong);
        assertTrue(unset >= 3 && set >= 1, unset + " not set, " + set + " set");
    }
}
