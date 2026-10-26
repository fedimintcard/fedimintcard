package io.github.bobodread876.fedimintcard.applet;

import javacard.framework.APDU;
import javacard.framework.Applet;
import javacard.framework.ISO7816;
import javacard.framework.ISOException;

/**
 * The applet that is instantiated first on the J3R180.
 *
 * cashucard/javacard found that the chip fails the INSTALL that both instantiates a
 * package for the first time and runs a heavy constructor (6F00 or no answer), and
 * takes the same applet once a small one has been instantiated from the package
 * before it. {@link FedimintCardApplet}'s constructor builds four EC key pairs, a key
 * generator and the signer, so it is that kind of heavy. This applet does nothing: it
 * answers its SELECT and refuses everything else, and is never spoken to again.
 *
 * Pattern from cashucard/javacard (MIT, (c) 2026 The Cashu Card authors, getfoxy).
 */
public final class Opener extends Applet {
    public static void install(byte[] bArray, short bOffset, byte bLength) {
        new Opener().register();
    }

    private Opener() {
    }

    public void process(APDU apdu) {
        if (selectingApplet()) {
            return;
        }
        ISOException.throwIt(ISO7816.SW_INS_NOT_SUPPORTED);
    }
}
