package io.github.bobodread876.fedimintcard.bridge;

import com.licel.jcardsim.base.SimulatorRuntime;
import com.licel.jcardsim.smartcardio.CardSimulator;
import com.licel.jcardsim.utils.AIDUtil;
import io.github.bobodread876.fedimintcard.applet.FedimintCardApplet;
import java.lang.reflect.Field;
import java.security.SecureRandom;
import javacard.framework.AID;
import javacard.framework.Applet;
import javacard.security.KeyPair;
import javax.smartcardio.CommandAPDU;
import javax.smartcardio.ResponseAPDU;

/** Owns the simulated card. */
final class SimCard {
    static final String AID_HEX = "F0464D4341524401";

    private static final class ExposedRuntime extends SimulatorRuntime {
        Applet appletAt(AID aid) { return getApplet(aid); }
    }

    private final boolean fixedKeys;
    private CardSimulator sim;

    SimCard(boolean fixedKeys) {
        this.fixedKeys = fixedKeys;
        reset();
    }

    /**
     * Fresh card. jCardSim's key generator is unseeded, so every new card would make the same
     * first key; unless fixedKeys is set we seed it from SecureRandom. Bridge only, never in the applet.
     */
    synchronized void reset() {
        ExposedRuntime rt = new ExposedRuntime();
        CardSimulator s = new CardSimulator(rt);
        AID aid = AIDUtil.create(AID_HEX);
        s.installApplet(aid, FedimintCardApplet.class);
        s.selectApplet(aid);
        if (!fixedKeys) {
            try {
                Field genF = FedimintCardApplet.class.getDeclaredField("gen");
                genF.setAccessible(true);
                KeyPair gen = (KeyPair) genF.get(rt.appletAt(aid));
                Field implF = KeyPair.class.getDeclaredField("impl");
                implF.setAccessible(true);
                Object impl = implF.get(gen);
                Field rndF = impl.getClass().getDeclaredField("rnd");
                rndF.setAccessible(true);
                byte[] seed = new byte[32];
                new SecureRandom().nextBytes(seed);
                ((SecureRandom) rndF.get(impl)).setSeed(seed);
            } catch (ReflectiveOperationException e) {
                throw new IllegalStateException("cannot seed the simulator's key generator", e);
            }
        }
        sim = s;
    }

    synchronized byte[] transmit(byte[] apdu) {
        ResponseAPDU r = sim.transmitCommand(new CommandAPDU(apdu));
        System.err.printf("INS %02x -> SW %04x%n", apdu[1] & 0xFF, r.getSW());
        return r.getBytes();
    }
}
