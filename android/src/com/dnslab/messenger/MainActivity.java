package com.dnslab.messenger;

import android.Manifest;
import android.app.Activity;
import android.content.pm.PackageManager;
import android.graphics.Color;
import android.graphics.Typeface;
import android.graphics.drawable.GradientDrawable;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.text.InputType;
import android.util.TypedValue;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.view.inputmethod.EditorInfo;
import android.widget.Button;
import android.widget.EditText;
import android.widget.FrameLayout;
import android.widget.LinearLayout;
import android.widget.RadioButton;
import android.widget.RadioGroup;
import android.widget.ScrollView;
import android.widget.TextView;
import android.widget.Toast;

import java.text.SimpleDateFormat;
import java.util.Date;
import java.util.Locale;

/**
 * The Android UI: plain platform views, one Activity, no AndroidX and no XML.
 *
 * <p>All protocol work happens in the Rust core; this class only renders state
 * and forwards taps. The UI is polled every 700 ms rather than pushed from
 * native threads, because JNI callbacks would need a JVM attach per event and
 * the screen repaints on a timer anyway.
 *
 * <p>Push-to-talk is a <em>toggle</em> here, not a hold: a finger holding the
 * screen cannot also read the transcript, and Android delivers touch as a
 * pointer that egui-style "is the button down" checks handled badly. Tap to
 * start, tap again to send.
 */
public class MainActivity extends Activity {
    private static final int BG = 0xFF101419;
    private static final int PANEL = 0xFF1A2027;
    private static final int TEXT = 0xFFE8ECF2;
    private static final int DIM = 0xFF8B95A3;
    private static final int ACCENT = 0xFF3B82F6;
    private static final int GOOD = 0xFF4ADE80;
    private static final int BAD = 0xFFF87171;
    private static final int OUT_BUBBLE = 0xFF1E3A8A;
    private static final int IN_BUBBLE = 0xFF232B35;

    private enum Screen { LIST, CHAT, SETUP, LOG }

    private long handle;
    private String configPath;

    private final Handler ui = new Handler(Looper.getMainLooper());
    private LinearLayout content;
    private TextView headerStatus;
    private TextView headerTitle;
    private Button navChats;
    private Button navSetup;
    private Button navLog;

    private Screen screen = Screen.LIST;
    private String peer = "";
    private String peerLabel = "";

    // chat widgets
    private LinearLayout bubbles;
    private ScrollView bubbleScroll;
    private EditText composer;
    private Button talk;
    private boolean talking;

    // setup widgets
    private EditText fName;
    private EditText fKey;
    private EditText fZone;
    private EditText fListen;
    private EditText fResolver;
    private EditText fDirect;
    private EditText fRelay;
    private RadioButton rDirect;
    private RadioButton rResolver;
    private String nodeId = "";
    private String voiceMode = "3200";
    private String voiceFrames = "12";
    private String voiceBuffer = "240";
    private String dbPath = "";
    private String downloadDir = "";

    // log widget
    private TextView logView;

    // caches so a rebuild only happens when the data actually changed
    private String lastContacts = "\u0000";
    private String lastMessages = "\u0000";
    private String lastLog = "\u0000";

    private final Runnable tick = new Runnable() {
        @Override
        public void run() {
            refresh();
            ui.postDelayed(this, 700);
        }
    };

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        setTheme(android.R.style.Theme_Material_NoActionBar);

        String files = getFilesDir().getAbsolutePath();
        configPath = Native.prepare(files);
        Native.init(this);
        handle = Native.start(configPath);

        buildChrome();
        showList();
        if (handle == 0) {
            toast("Node failed to start - see the Log tab");
        }
        ui.postDelayed(tick, 400);
    }

    @Override
    protected void onDestroy() {
        ui.removeCallbacks(tick);
        if (handle != 0) {
            Native.stop(handle);
            handle = 0;
        }
        super.onDestroy();
    }

    @Override
    public void onBackPressed() {
        if (screen == Screen.CHAT) {
            showList();
        } else {
            super.onBackPressed();
        }
    }

    // ---------------------------------------------------------------- chrome

    private void buildChrome() {
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setBackgroundColor(BG);

        LinearLayout header = new LinearLayout(this);
        header.setOrientation(LinearLayout.VERTICAL);
        header.setPadding(dp(16), dp(12), dp(16), dp(10));
        header.setBackgroundColor(PANEL);

        headerTitle = new TextView(this);
        headerTitle.setText("DNS Lab");
        headerTitle.setTextColor(TEXT);
        headerTitle.setTextSize(20f);
        headerTitle.setTypeface(Typeface.DEFAULT_BOLD);
        header.addView(headerTitle);

        headerStatus = new TextView(this);
        headerStatus.setTextColor(DIM);
        headerStatus.setTextSize(12f);
        headerStatus.setPadding(0, dp(3), 0, 0);
        header.addView(headerStatus);
        root.addView(header, new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));

        content = new LinearLayout(this);
        content.setOrientation(LinearLayout.VERTICAL);
        root.addView(content, new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f));

        LinearLayout nav = new LinearLayout(this);
        nav.setOrientation(LinearLayout.HORIZONTAL);
        nav.setBackgroundColor(PANEL);
        nav.setPadding(dp(4), dp(4), dp(4), dp(4));
        navChats = navButton("Chats");
        navSetup = navButton("Setup");
        navLog = navButton("Log");
        navChats.setOnClickListener(new View.OnClickListener() {
            public void onClick(View v) { showList(); }
        });
        navSetup.setOnClickListener(new View.OnClickListener() {
            public void onClick(View v) { showSetup(); }
        });
        navLog.setOnClickListener(new View.OnClickListener() {
            public void onClick(View v) { showLog(); }
        });
        nav.addView(navChats, new LinearLayout.LayoutParams(0, dp(48), 1f));
        nav.addView(navSetup, new LinearLayout.LayoutParams(0, dp(48), 1f));
        nav.addView(navLog, new LinearLayout.LayoutParams(0, dp(48), 1f));
        root.addView(nav, new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));

        setContentView(root);
    }

    private Button navButton(String label) {
        Button b = new Button(this);
        b.setText(label);
        b.setAllCaps(false);
        b.setTextSize(15f);
        b.setBackgroundColor(Color.TRANSPARENT);
        b.setTextColor(DIM);
        return b;
    }

    private void selectNav(Button active) {
        navChats.setTextColor(active == navChats ? ACCENT : DIM);
        navSetup.setTextColor(active == navSetup ? ACCENT : DIM);
        navLog.setTextColor(active == navLog ? ACCENT : DIM);
    }

    // --------------------------------------------------------------- screens

    private void showList() {
        screen = Screen.LIST;
        talking = false;
        composer = null;
        bubbles = null;
        bubbleScroll = null;
        talk = null;
        selectNav(navChats);
        headerTitle.setText("Chats");
        content.removeAllViews();

        ScrollView scroll = new ScrollView(this);
        LinearLayout list = new LinearLayout(this);
        list.setOrientation(LinearLayout.VERTICAL);
        list.setId(View.generateViewId());
        list.setTag("contacts");
        list.setPadding(dp(8), dp(8), dp(8), dp(8));
        scroll.addView(list, new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));
        content.addView(scroll, new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f));
        lastContacts = "\u0000";
        refresh();
    }

    private void openChat(String id, String name) {
        peer = id;
        peerLabel = name;
        screen = Screen.CHAT;
        selectNav(null);
        headerTitle.setText(name.isEmpty() ? id : name);
        content.removeAllViews();

        LinearLayout column = new LinearLayout(this);
        column.setOrientation(LinearLayout.VERTICAL);

        LinearLayout bar = new LinearLayout(this);
        bar.setOrientation(LinearLayout.HORIZONTAL);
        bar.setPadding(dp(8), dp(6), dp(8), dp(6));
        bar.setGravity(Gravity.CENTER_VERTICAL);
        Button back = new Button(this);
        back.setText("\u2039 Back");
        back.setAllCaps(false);
        back.setTextColor(TEXT);
        back.setBackgroundColor(Color.TRANSPARENT);
        back.setOnClickListener(new View.OnClickListener() {
            public void onClick(View v) { showList(); }
        });
        bar.addView(back);
        TextView who = new TextView(this);
        who.setText(id);
        who.setTextColor(DIM);
        who.setTextSize(12f);
        who.setPadding(dp(6), 0, 0, 0);
        bar.addView(who);
        column.addView(bar);

        bubbleScroll = new ScrollView(this);
        bubbles = new LinearLayout(this);
        bubbles.setOrientation(LinearLayout.VERTICAL);
        bubbles.setPadding(dp(10), dp(6), dp(10), dp(6));
        bubbleScroll.addView(bubbles, new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));
        column.addView(bubbleScroll, new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f));

        LinearLayout compose = new LinearLayout(this);
        compose.setOrientation(LinearLayout.HORIZONTAL);
        compose.setPadding(dp(8), dp(6), dp(8), dp(6));
        compose.setBackgroundColor(PANEL);
        composer = new EditText(this);
        composer.setHint("Message");
        composer.setTextColor(TEXT);
        composer.setHintTextColor(DIM);
        composer.setTextSize(16f);
        composer.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_CAP_SENTENCES);
        composer.setImeOptions(EditorInfo.IME_ACTION_SEND);
        composer.setSingleLine(true);
        composer.setBackgroundColor(IN_BUBBLE);
        composer.setPadding(dp(12), dp(10), dp(12), dp(10));
        composer.setOnEditorActionListener(new TextView.OnEditorActionListener() {
            @Override
            public boolean onEditorAction(TextView v, int actionId, android.view.KeyEvent e) {
                sendCurrent();
                return true;
            }
        });
        compose.addView(composer, new LinearLayout.LayoutParams(0,
                ViewGroup.LayoutParams.WRAP_CONTENT, 1f));
        Button send = new Button(this);
        send.setText("Send");
        send.setAllCaps(false);
        send.setTextColor(Color.WHITE);
        send.setBackgroundColor(ACCENT);
        send.setOnClickListener(new View.OnClickListener() {
            public void onClick(View v) { sendCurrent(); }
        });
        LinearLayout.LayoutParams sendLp = new LinearLayout.LayoutParams(dp(84), dp(52));
        sendLp.leftMargin = dp(8);
        compose.addView(send, sendLp);
        column.addView(compose);

        talk = new Button(this);
        talk.setAllCaps(false);
        talk.setTextSize(16f);
        updateTalkButton();
        talk.setOnClickListener(new View.OnClickListener() {
            public void onClick(View v) { toggleTalk(); }
        });
        LinearLayout.LayoutParams talkLp = new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, dp(56));
        talkLp.setMargins(dp(8), 0, dp(8), dp(8));
        column.addView(talk, talkLp);

        content.addView(column, new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f));
        lastMessages = "\u0000";
        refresh();
    }

    private void showSetup() {
        screen = Screen.SETUP;
        selectNav(navSetup);
        headerTitle.setText("Setup");
        content.removeAllViews();
        talking = false;
        composer = null;
        bubbles = null;
        talk = null;

        ScrollView scroll = new ScrollView(this);
        LinearLayout form = new LinearLayout(this);
        form.setOrientation(LinearLayout.VERTICAL);
        form.setPadding(dp(14), dp(10), dp(14), dp(20));

        String[] cfg = Native.fields(Native.configGet(handle));
        if (cfg.length >= 14) {
            nodeId = cfg[1];
            voiceMode = cfg[9];
            voiceFrames = cfg[10];
            voiceBuffer = cfg[11];
            dbPath = cfg[12];
            downloadDir = cfg[13];
        }

        form.addView(sectionLabel("This device"));
        TextView idView = new TextView(this);
        idView.setText("node " + (cfg.length > 1 ? cfg[1] : "?")
                + "   tag " + (cfg.length > 1 ? snapshotTag() : "?"));
        idView.setTextColor(DIM);
        idView.setTextSize(13f);
        idView.setPadding(0, dp(2), 0, dp(10));
        form.addView(idView);

        fName = field(form, "Display name", cfg.length > 0 ? cfg[0] : "");
        fKey = field(form, "Shared key (same on every node)", cfg.length > 8 ? cfg[8] : "");
        fKey.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD);
        fZone = field(form, "Zone / suffix", cfg.length > 2 ? cfg[2] : "lab.local");
        fListen = field(form, "Listen address", cfg.length > 3 ? cfg[3] : "0.0.0.0:1054");

        form.addView(sectionLabel("Transport"));
        RadioGroup g = new RadioGroup(this);
        g.setOrientation(RadioGroup.HORIZONTAL);
        rDirect = new RadioButton(this);
        rDirect.setText("direct (peer to peer)");
        rDirect.setTextColor(TEXT);
        rResolver = new RadioButton(this);
        rResolver.setText("resolver (tunnel via DNS)");
        rResolver.setTextColor(TEXT);
        g.addView(rDirect);
        g.addView(rResolver);
        if (cfg.length > 4 && cfg[4].equals("resolver")) {
            rResolver.setChecked(true);
        } else {
            rDirect.setChecked(true);
        }
        form.addView(g);

        fResolver = field(form, "Resolver address (resolver mode)", cfg.length > 5 ? cfg[5] : "");
        fDirect = field(form, "Direct / relay address", cfg.length > 6 ? cfg[6] : "");
        fRelay = field(form, "Poll relay (mailbox)", cfg.length > 7 ? cfg[7] : "");

        Button save = new Button(this);
        save.setText("Save and restart node");
        save.setAllCaps(false);
        save.setTextColor(Color.WHITE);
        save.setBackgroundColor(ACCENT);
        save.setOnClickListener(new View.OnClickListener() {
            public void onClick(View v) { saveSetup(); }
        });
        LinearLayout.LayoutParams saveLp = new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, dp(52));
        saveLp.topMargin = dp(8);
        form.addView(save, saveLp);

        form.addView(sectionLabel("Add a contact"));
        final EditText cId = field(form, "Node id (8 hex)", "");
        final EditText cName = field(form, "Name", "");
        final EditText cAddr = field(form, "Address ip:port (direct)", "");
        final RadioGroup via = new RadioGroup(this);
        via.setOrientation(RadioGroup.HORIZONTAL);
        final RadioButton vDirect = new RadioButton(this);
        vDirect.setText("direct");
        vDirect.setTextColor(TEXT);
        final RadioButton vRelay = new RadioButton(this);
        vRelay.setText("relay");
        vRelay.setTextColor(TEXT);
        via.addView(vDirect);
        via.addView(vRelay);
        vDirect.setChecked(true);
        form.addView(via);

        Button add = new Button(this);
        add.setText("Add contact");
        add.setAllCaps(false);
        add.setTextColor(TEXT);
        add.setBackgroundColor(PANEL);
        add.setOnClickListener(new View.OnClickListener() {
            public void onClick(View v) {
                String id = cId.getText().toString().trim();
                if (id.isEmpty()) {
                    toast("Contact id is required");
                    return;
                }
                boolean ok = Native.addContact(handle, id,
                        cName.getText().toString().trim(),
                        cAddr.getText().toString().trim(),
                        vDirect.isChecked() ? "direct" : "relay");
                toast(ok ? "Contact added" : "Could not add contact");
                cId.setText("");
                cName.setText("");
                cAddr.setText("");
            }
        });
        LinearLayout.LayoutParams addLp = new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, dp(48));
        addLp.topMargin = dp(6);
        form.addView(add, addLp);

        scroll.addView(form);
        content.addView(scroll, new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f));
    }

    private void showLog() {
        screen = Screen.LOG;
        selectNav(navLog);
        headerTitle.setText("Log");
        content.removeAllViews();
        talking = false;
        composer = null;
        bubbles = null;
        talk = null;

        ScrollView scroll = new ScrollView(this);
        logView = new TextView(this);
        logView.setTextColor(DIM);
        logView.setTextSize(11f);
        logView.setTypeface(Typeface.MONOSPACE);
        logView.setPadding(dp(10), dp(8), dp(10), dp(10));
        scroll.addView(logView);
        content.addView(scroll, new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f));
        lastLog = "\u0000";
        refresh();
    }

    // -------------------------------------------------------------- actions

    private void sendCurrent() {
        if (composer == null || handle == 0) {
            return;
        }
        String body = composer.getText().toString().trim();
        if (body.isEmpty()) {
            return;
        }
        if (Native.send(handle, peer, body)) {
            composer.setText("");
            refresh();
        } else {
            toast("Message not queued");
        }
    }

    private void toggleTalk() {
        if (handle == 0) {
            return;
        }
        if (talking) {
            Native.voiceStop(handle);
            talking = false;
            updateTalkButton();
            return;
        }
        if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(new String[]{Manifest.permission.RECORD_AUDIO}, 7);
            return;
        }
        if (Native.voiceStart(handle, peer)) {
            talking = true;
            updateTalkButton();
        } else {
            toast("Microphone unavailable - check app permissions");
        }
    }

    @Override
    public void onRequestPermissionsResult(int code, String[] perms, int[] granted) {
        if (code == 7 && granted.length > 0 && granted[0] == PackageManager.PERMISSION_GRANTED) {
            toggleTalk();
        } else if (code == 7) {
            toast("Microphone permission denied in system settings");
        }
    }

    private void updateTalkButton() {
        if (talk == null) {
            return;
        }
        talk.setText(talking ? "Talking - tap to send" : "Tap to talk");
        talk.setTextColor(Color.WHITE);
        talk.setBackgroundColor(talking ? 0xFFB91C1C : 0xFF2B3440);
    }

    private void saveSetup() {
        String egress = rResolver.isChecked() ? "resolver" : "direct";
        String[] parts = new String[]{
                fName.getText().toString().trim(),
                nodeId,
                fZone.getText().toString().trim(),
                fListen.getText().toString().trim(),
                egress,
                fResolver.getText().toString().trim(),
                fDirect.getText().toString().trim(),
                fRelay.getText().toString().trim(),
                fKey.getText().toString(),
                voiceMode,
                voiceFrames,
                voiceBuffer,
                dbPath,
                downloadDir,
        };
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < parts.length; i++) {
            if (i > 0) {
                sb.append('\u001f');
            }
            sb.append(parts[i].replace('\u001f', ' ').replace('\n', ' '));
        }
        boolean ok = Native.configSet(handle, sb.toString());
        toast(ok ? "Saved - node restarted" : "Save failed");
        if (ok) {
            refresh();
        }
    }

    // -------------------------------------------------------------- refresh

    private void refresh() {
        if (handle == 0) {
            headerStatus.setText("node stopped");
            return;
        }
        String[] snap = Native.fields(Native.snapshot(handle));
        if (snap.length >= 5) {
            headerStatus.setText("me " + snap[0] + "  tag " + snap[1]
                    + (snap[4].equals("1") ? "  receiving voice" : ""));
        }
        if (screen == Screen.LIST) {
            renderContacts();
        } else if (screen == Screen.CHAT) {
            renderMessages();
        } else if (screen == Screen.LOG) {
            renderLog();
        }
    }

    private String snapshotTag() {
        String[] snap = Native.fields(Native.snapshot(handle));
        return snap.length > 1 ? snap[1] : "?";
    }

    private void renderContacts() {
        String reply = Native.contacts(handle);
        if (reply.equals(lastContacts)) {
            return;
        }
        lastContacts = reply;
        LinearLayout list = findContacts();
        if (list == null) {
            return;
        }
        list.removeAllViews();
        String[] rows = Native.records(reply);
        if (rows.length == 0) {
            TextView empty = new TextView(this);
            empty.setText("No contacts yet.\n\nSetup tab -> Add a contact "
                    + "(the peer's node id, and its ip:port for direct mode).");
            empty.setTextColor(DIM);
            empty.setTextSize(14f);
            empty.setPadding(dp(8), dp(12), dp(8), dp(8));
            list.addView(empty);
            return;
        }
        for (String row : rows) {
            final String[] f = Native.fields(row);
            if (f.length < 2) {
                continue;
            }
            final String id = f[0];
            final String name = f[1];
            String last = f.length > 2 ? f[2] : "";
            long ts = f.length > 3 ? parseLong(f[3]) : 0;
            String addr = f.length > 4 ? f[4] : "";

            LinearLayout item = new LinearLayout(this);
            item.setOrientation(LinearLayout.VERTICAL);
            item.setBackgroundColor(PANEL);
            item.setPadding(dp(14), dp(12), dp(14), dp(12));
            item.setClickable(true);
            item.setOnClickListener(new View.OnClickListener() {
                public void onClick(View v) { openChat(id, name); }
            });

            TextView title = new TextView(this);
            title.setText(name);
            title.setTextColor(TEXT);
            title.setTextSize(16f);
            title.setTypeface(Typeface.DEFAULT_BOLD);
            item.addView(title);

            TextView sub = new TextView(this);
            String subText;
            if (!last.isEmpty()) {
                subText = last + (ts > 0 ? "   " + clock(ts) : "");
            } else if (!addr.isEmpty()) {
                subText = addr;
            } else {
                subText = id;
            }
            sub.setText(subText);
            sub.setTextColor(DIM);
            sub.setTextSize(13f);
            sub.setSingleLine(true);
            item.addView(sub);

            LinearLayout.LayoutParams lp = new LinearLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT);
            lp.bottomMargin = dp(8);
            list.addView(item, lp);
        }
    }

    private LinearLayout findContacts() {
        for (int i = 0; i < content.getChildCount(); i++) {
            View v = content.getChildAt(i);
            if (v instanceof ScrollView) {
                ViewGroup g = (ViewGroup) ((ScrollView) v).getChildAt(0);
                if (g != null && "contacts".equals(g.getTag())) {
                    return (LinearLayout) g;
                }
            }
        }
        return null;
    }

    private void renderMessages() {
        String reply = Native.messages(handle, peer, 200);
        if (reply.equals(lastMessages)) {
            return;
        }
        boolean atBottom = bubbleScroll != null
                && bubbleScroll.getChildAt(0) != null
                && bubbleScroll.getScrollY() + bubbleScroll.getHeight()
                >= bubbleScroll.getChildAt(0).getHeight() - dp(40);
        lastMessages = reply;
        if (bubbles == null) {
            return;
        }
        bubbles.removeAllViews();
        String[] rows = Native.records(reply);
        if (rows.length == 0) {
            TextView empty = new TextView(this);
            empty.setText("No messages yet.");
            empty.setTextColor(DIM);
            empty.setTextSize(14f);
            empty.setPadding(dp(6), dp(10), dp(6), dp(10));
            bubbles.addView(empty);
            return;
        }
        for (String row : rows) {
            String[] f = Native.fields(row);
            if (f.length < 5) {
                continue;
            }
            boolean out = "out".equals(f[1]);
            String status = f[2];
            String body = f[4];

            LinearLayout bubble = new LinearLayout(this);
            bubble.setOrientation(LinearLayout.VERTICAL);
            bubble.setBackgroundColor(out ? OUT_BUBBLE : IN_BUBBLE);
            bubble.setPadding(dp(12), dp(8), dp(12), dp(8));

            TextView text = new TextView(this);
            text.setText(body);
            text.setTextColor(TEXT);
            text.setTextSize(15f);
            bubble.addView(text);

            if (out) {
                TextView state = new TextView(this);
                state.setText(status);
                state.setTextSize(10f);
                state.setTextColor("delivered".equals(status) ? GOOD
                        : ("failed".equals(status) ? BAD : DIM));
                state.setPadding(0, dp(2), 0, 0);
                bubble.addView(state);
            }

            LinearLayout rowWrap = new LinearLayout(this);
            rowWrap.setOrientation(LinearLayout.HORIZONTAL);
            rowWrap.setGravity(out ? Gravity.END : Gravity.START);
            LinearLayout.LayoutParams blp = new LinearLayout.LayoutParams(
                    ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT);
            blp.bottomMargin = dp(6);
            rowWrap.addView(bubble, blp);
            bubbles.addView(rowWrap);
        }
        if (atBottom && bubbleScroll != null) {
            bubbleScroll.post(new Runnable() {
                public void run() { bubbleScroll.fullScroll(View.FOCUS_DOWN); }
            });
        }
    }

    private void renderLog() {
        String text = Native.logTail(handle, 200);
        if (text.equals(lastLog) || logView == null) {
            return;
        }
        lastLog = text;
        logView.setText(text);
    }

    // ---------------------------------------------------------------- utils

    private EditText field(LinearLayout parent, String label, String value) {
        TextView l = new TextView(this);
        l.setText(label);
        l.setTextColor(DIM);
        l.setTextSize(12f);
        l.setPadding(0, dp(8), 0, dp(2));
        parent.addView(l);

        EditText e = new EditText(this);
        e.setText(value);
        e.setTextColor(TEXT);
        e.setTextSize(15f);
        e.setSingleLine(true);
        e.setBackgroundColor(PANEL);
        e.setPadding(dp(10), dp(8), dp(10), dp(8));
        parent.addView(e);
        return e;
    }

    private TextView sectionLabel(String text) {
        TextView t = new TextView(this);
        t.setText(text.toUpperCase(Locale.US));
        t.setTextColor(ACCENT);
        t.setTextSize(11f);
        t.setTypeface(Typeface.DEFAULT_BOLD);
        t.setPadding(0, dp(18), 0, dp(4));
        return t;
    }

    private int dp(float v) {
        return (int) TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_DIP, v,
                getResources().getDisplayMetrics());
    }

    private void toast(String msg) {
        Toast.makeText(this, msg, Toast.LENGTH_SHORT).show();
    }

    private static long parseLong(String s) {
        try {
            return Long.parseLong(s.trim());
        } catch (NumberFormatException e) {
            return 0;
        }
    }

    private static String clock(long epochSecs) {
        return new SimpleDateFormat("HH:mm", Locale.US).format(new Date(epochSecs * 1000L));
    }
}
