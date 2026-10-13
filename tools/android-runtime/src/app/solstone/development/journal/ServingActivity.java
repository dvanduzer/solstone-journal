// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
package app.solstone.development.journal;

import android.app.Activity;
import android.content.*;
import android.net.*;
import android.os.*;
import android.view.WindowManager;
import android.widget.*;
import org.json.*;
import java.io.*;
import java.net.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.concurrent.*;

/** Development owner controls. Journal requests arrive only at the SPL door. */
public class ServingActivity extends Activity {
    volatile Messenger worker;
    TextView status;
    FileObserver commands;
    int sequence;
    boolean bound;
    String root;
    final Handler ui = new Handler(Looper.getMainLooper());
    final ExecutorService control = Executors.newSingleThreadExecutor();
    final ConcurrentHashMap<Integer, CompletableFuture<JSONObject>> pending = new ConcurrentHashMap<>();
    final Messenger replies = new Messenger(new Handler(Looper.getMainLooper()) {
        public void handleMessage(Message message) {
            CompletableFuture<JSONObject> future = pending.remove(message.arg1);
            if (future != null) {
                Bundle b = message.getData();
                JSONObject row = MainActivity.obj("native", MainActivity.parse(b.getString("result")),
                    "pid", b.getInt("pid"), "uid", b.getInt("uid"), "pss_kb", b.getInt("pss_kb"),
                    "java_heap", b.getLong("java_heap"), "native_ms", b.getLong("nanos") / 1e6);
                future.complete(row);
            }
        }
    });
    final ServiceConnection connection = new ServiceConnection() {
        public void onServiceConnected(ComponentName name, IBinder binder) {
            worker = new Messenger(binder);
            control.execute(()->{
                try { show(call(MainActivity.obj("op", "start", "address", wifiAddress()+":7657", "onnx_library", getApplicationInfo().nativeLibraryDir+"/libonnxruntime.so"))); }
                catch(Exception e) { show(MainActivity.obj("error", e.toString())); }
                try { show(call(MainActivity.obj("op", "audio-enable"))); }
                catch(Exception e) { show(MainActivity.obj("error", e.toString())); }
                consumeCommand();
            });
        }
        public void onServiceDisconnected(ComponentName name) {
            worker = null;
            for (CompletableFuture<JSONObject> future: pending.values()) future.completeExceptionally(new EOFException("worker lost"));
            pending.clear();
            show(MainActivity.obj("error", "worker disconnected"));
        }
    };
    String wifiAddress() throws Exception {
        ConnectivityManager cm = (ConnectivityManager)getSystemService(CONNECTIVITY_SERVICE);
        for (Network network: cm.getAllNetworks()) {
            NetworkCapabilities caps = cm.getNetworkCapabilities(network);
            LinkProperties links = cm.getLinkProperties(network);
            if (caps == null || links == null || !caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)) continue;
            for (LinkAddress link: links.getLinkAddresses()) {
                InetAddress address = link.getAddress();
                if (address instanceof Inet4Address && address.isSiteLocalAddress()) return address.getHostAddress();
            }
        }
        throw new IOException("private Wi-Fi IPv4 unavailable");
    }
    JSONObject call(JSONObject command) throws Exception {
        Messenger target = worker;
        if (target == null) throw new IOException("worker unavailable");
        int id = ++sequence;
        CompletableFuture<JSONObject> result = new CompletableFuture<>(); pending.put(id, result);
        Message message = Message.obtain(null, 1); message.arg1 = id; message.replyTo = replies;
        Bundle b = new Bundle(); b.putString("root", root); b.putString("op", "serve:"+command); b.putInt("count", 1); message.setData(b);
        try { target.send(message); return result.get(90, TimeUnit.SECONDS); }
        finally { pending.remove(id); }
    }
    void show(JSONObject row) { ui.post(()->status.setText(row.toString())); }
    void unpackPayload(String asset, File destination) throws IOException {
        String[] children = getAssets().list(asset);
        if (children.length > 0) {
            if (!destination.mkdirs() && !destination.isDirectory()) throw new IOException("payload directory unavailable");
            for (String child: children) unpackPayload(asset+"/"+child, new File(destination, child));
        } else {
            try(InputStream input = getAssets().open(asset); FileOutputStream output = new FileOutputStream(destination)) {
                byte[] buffer = new byte[16384]; int n;
                while((n = input.read(buffer)) != -1) output.write(buffer, 0, n);
            }
        }
    }
    // Debug USB automation writes this file through run-as, which has the app UID.
    // Intent extras and external IPC never carry owner requests.
    void consumeCommand() {
        File command = new File(getFilesDir(), "serving-command.json");
        if (!command.isFile()) return;
        String id = "unread";
        JSONObject row;
        try {
            JSONObject input = new JSONObject(new String(Files.readAllBytes(command.toPath()), StandardCharsets.UTF_8));
            id = input.getString("id");
            if (!command.delete()) throw new IOException("command retirement failed");
            row = call(input);
        } catch(Exception e) { row = MainActivity.obj("error", e.toString()); }
        try {
            row.put("id", id); row.put("host_pid", android.os.Process.myPid());
            File stage = new File(getFilesDir(), "serving-reply.stage");
            try(FileOutputStream output = new FileOutputStream(stage)) {
                output.write(row.toString().getBytes(StandardCharsets.UTF_8)); output.getFD().sync();
            }
            Files.move(stage.toPath(), new File(getFilesDir(), "serving-reply.json").toPath(), java.nio.file.StandardCopyOption.REPLACE_EXISTING);
            show(row);
        } catch(Exception e) { android.util.Log.e("JournalServing", "reply write failed", e); }
    }
    public void onCreate(Bundle state) {
        super.onCreate(state); getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        root = new File(getFilesDir(), "synthetic-serving-journal").toString();
        new File(root).mkdirs();
        LinearLayout controls = new LinearLayout(this); controls.setOrientation(LinearLayout.VERTICAL);
        status = new TextView(this); status.setTextSize(16);
        for(String op: new String[]{"status", "index", "stop"}) {
            Button button = new Button(this); button.setText(op);
            button.setOnClickListener(v->control.execute(()->{ try { show(call(MainActivity.obj("op", op))); } catch(Exception e) { show(MainActivity.obj("error", e.toString())); } }));
            controls.addView(button);
        }
        controls.addView(status); setContentView(controls);
        commands = new FileObserver(getFilesDir(), FileObserver.MOVED_TO | FileObserver.CLOSE_WRITE) {
            public void onEvent(int event, String path) {
                if ("serving-command.json".equals(path)) control.execute(()->consumeCommand());
            }
        };
        commands.startWatching();
        try {
            unpackPayload("journal-payload", new File(getFilesDir(), "journal-payload"));
            bound = bindService(new Intent(this, JournalService.class), connection, Context.BIND_AUTO_CREATE);
        } catch(IOException e) { show(MainActivity.obj("error", e.toString())); }
    }
    protected void onDestroy() {
        commands.stopWatching(); if(bound) unbindService(connection); control.shutdownNow(); super.onDestroy();
    }
}
