package com.dnslab.messenger;

/**
 * Host-side smoke test for the JNI bridge (no Android, no emulator).
 *
 * <p>Build the library with the bridge enabled and run this on a desktop JVM:
 *
 * <pre>
 *   cargo build --features jni-bridge --lib
 *   javac -d /tmp/harness android/src/com/dnslab/messenger/Native.java \
 *         android/test/NativeHarness.java
 *   java -Djava.library.path=target/debug -cp /tmp/harness \
 *        com.dnslab.messenger.NativeHarness /tmp/bridge-test
 * </pre>
 *
 * <p>It starts a node from the config in the given directory, adds a contact,
 * sends one message and waits for the delivery receipt. {@code
 * scripts/android_bridge_test.sh} drives it end to end against a real peer node.
 */
public final class NativeHarness {
    private NativeHarness() {}

    public static void main(String[] args) throws Exception {
        if (args.length < 3) {
            System.err.println("usage: NativeHarness <dir> <peer-id> <peer-addr>");
            System.exit(2);
        }
        String dir = args[0];
        String peerId = args[1];
        String peerAddr = args[2];

        String path = Native.prepare(dir);
        System.out.println("config  " + path);
        long h = Native.start(path);
        if (h == 0) {
            System.err.println("FAIL: start() returned 0");
            System.exit(1);
        }
        String[] snap = Native.fields(Native.snapshot(h));
        System.out.println("node    " + snap[0] + "  tag " + snap[1]);
        if (!"1111aaaa".equals(snap[0])) {
            System.err.println("FAIL: node id is not the one in the config file");
            System.exit(1);
        }

        String[] cfg = Native.fields(Native.configGet(h));
        System.out.println("zone    " + cfg[2] + "  listen " + cfg[3] + "  egress " + cfg[4]);
        if (!Native.addContact(h, peerId, "peer", peerAddr, "direct")) {
            System.err.println("FAIL: addContact");
            System.exit(1);
        }
        String contacts = Native.contacts(h);
        if (!contacts.contains(peerId)) {
            System.err.println("FAIL: contact not listed: " + contacts);
            System.exit(1);
        }

        String body = "hello from the java bridge";
        if (!Native.send(h, peerId, body)) {
            System.err.println("FAIL: send rejected");
            System.exit(1);
        }
        System.out.println("sent    " + body);

        String status = "";
        for (int i = 0; i < 60; i++) {
            String reply = Native.messages(h, peerId, 20);
            for (String rec : Native.records(reply)) {
                String[] f = Native.fields(rec);
                if (f.length >= 5 && body.equals(f[4])) {
                    status = f[2];
                }
            }
            if ("delivered".equals(status)) {
                break;
            }
            Thread.sleep(250);
        }
        System.out.println("status  " + status);
        if (!"delivered".equals(status)) {
            System.out.println("--- log ---");
            System.out.println(Native.logTail(h, 40));
            System.err.println("FAIL: no delivery receipt");
            Native.stop(h);
            System.exit(1);
        }
        Native.stop(h);
        System.out.println("PASS");
    }
}
