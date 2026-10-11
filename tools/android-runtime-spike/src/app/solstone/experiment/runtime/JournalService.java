// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
package app.solstone.experiment.runtime;
import android.app.Service;
import android.content.Intent;
import android.os.*;
import android.os.Process;
import org.json.*;
import java.util.concurrent.*;
import java.io.*;
import java.nio.charset.StandardCharsets;
public class JournalService extends Service {
    final ExecutorService work=Executors.newSingleThreadExecutor();
    Messenger incoming;
    public void onCreate() {
        super.onCreate();
        incoming=new Messenger(new Handler(Looper.getMainLooper()) {
            public void handleMessage(Message m) {
                final String op=m.getData().getString("op");
                final String root=m.getData().getString("root");
                final int count=m.getData().getInt("count",1), id=m.arg1;
                final Messenger reply=m.replyTo;
                if (op.equals("cancel") || op.equals("begin")) { respond(reply,id,Native.run(root,op,count),0); return; }
                work.execute(()->{
                    long start=SystemClock.elapsedRealtimeNanos();
                    try { respond(reply,id,Native.run(root,op,count),SystemClock.elapsedRealtimeNanos()-start); }
                    catch(Throwable e) { respond(reply,id,"{\"ok\":false,\"error\": "+JSONObject.quote(e.toString())+"}",SystemClock.elapsedRealtimeNanos()-start); }
                });
            }
        });
    }
    void respond(Messenger to,int id,String result,long nanos) {
        Message m=Message.obtain(null,1); m.arg1=id;
        Bundle b=new Bundle(); b.putString("result",result); b.putLong("nanos",nanos); b.putInt("pid",Process.myPid()); b.putInt("uid",Process.myUid()); b.putInt("threads",Thread.activeCount());
        Debug.MemoryInfo mem=new Debug.MemoryInfo(); Debug.getMemoryInfo(mem); b.putInt("pss_kb",mem.getTotalPss()); b.putLong("java_heap",Runtime.getRuntime().totalMemory()-Runtime.getRuntime().freeMemory());
        m.setData(b);
        evidence("response",result);
        try { to.send(m); } catch(RemoteException e) { evidence("reply_connection_lost",e.toString()); android.util.Log.i("RuntimeSpike","reply connection lost: "+e); }
    }
    synchronized void evidence(String name,String detail) {
        if(Process.myUid()!=getApplicationInfo().uid)return;
        try(FileOutputStream f=new FileOutputStream(new File(getFilesDir(),"worker-evidence.jsonl"),true)) {
            JSONObject row=new JSONObject(); row.put("event",name);row.put("detail",detail);row.put("pid",Process.myPid());row.put("elapsed_ms",SystemClock.elapsedRealtime());
            f.write((row.toString()+"\n").getBytes(StandardCharsets.UTF_8));f.getFD().sync();
        }catch(Exception e){android.util.Log.e("RuntimeSpike","worker evidence write failed",e);}
    }
    public IBinder onBind(Intent i) { return incoming.getBinder(); }
    public void onDestroy() { evidence("service_destroyed","last binding released"); Native.run("","cancel",1);work.shutdownNow(); super.onDestroy(); }
}
