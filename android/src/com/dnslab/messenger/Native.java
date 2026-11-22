package com.dnslab.messenger;

/**
 * Native surface of the Rust core (src/bridge.rs).
 *
 * <p>Every string returned is a list of records: fields separated by U+001F
 * (unit separator), records by newline, with U+001E standing in for a newline
 * inside a field. {@link #fields} turns one record into a String[].
 */
final class Native {
    private Native() {}

    static {
        System.loadLibrary("dns_lab");
    }

    /** Hand the Activity to cpal's AAudio backend; call before {@link #start}. */
    static native boolean init(Object activity);

    /** Config path for a files dir, creating it on first run. */
    static native String prepare(String filesDir);

    /** Opens the node (listeners, pollers, DB). Returns a handle, 0 on failure. */
    static native long start(String configPath);

    static native void stop(long handle);

    /** node_id, tag, status, talking, playing, display_name. */
    static native String snapshot(long handle);

    /** id, name, last body, last timestamp - one record per conversation. */
    static native String contacts(long handle);

    /** id, direction, status, timestamp, body - oldest first. */
    static native String messages(long handle, String peer, int limit);

    static native boolean send(long handle, String to, String body);

    static native boolean addContact(long handle, String id, String name, String addr, String via);

    /** display_name, node_id, zone, listen_addr, egress, resolver, direct_server,
     *  relay, shared_key, voice_mode, voice_frames, voice_buffer, db, downloads. */
    static native String configGet(long handle);

    static native boolean configSet(long handle, String data);

    static native boolean voiceStart(long handle, String peer);

    static native boolean voiceStop(long handle);

    static native String logTail(long handle, int lines);

    /** Split one record into its fields, restoring embedded newlines. */
    static String[] fields(String record) {
        String[] parts = record.split("\u001f", -1);
        for (int i = 0; i < parts.length; i++) {
            parts[i] = parts[i].replace('\u001e', '\n');
        }
        return parts;
    }

    /** Split a whole reply into records, dropping the trailing empty one. */
    static String[] records(String reply) {
        if (reply == null || reply.isEmpty()) {
            return new String[0];
        }
        String[] lines = reply.split("\n", -1);
        int n = 0;
        for (String line : lines) {
            if (!line.isEmpty()) {
                n++;
            }
        }
        String[] out = new String[n];
        int i = 0;
        for (String line : lines) {
            if (!line.isEmpty()) {
                out[i++] = line;
            }
        }
        return out;
    }
}
