// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
package app.solstone.development.journal;
import android.app.Activity;
import android.content.*;
import android.os.*;
import android.os.Process;
import android.view.WindowManager;
import android.widget.*;
import org.json.*;
import java.io.*;
import java.nio.charset.StandardCharsets;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;

public class MainActivity extends Activity {
    final Handler ui=new Handler(Looper.getMainLooper());
    final ConcurrentHashMap<Integer,CompletableFuture<JSONObject>> pending=new ConcurrentHashMap<>();
    final AtomicInteger next=new AtomicInteger();
    final ArrayList<Long> ticks=new ArrayList<>();
    volatile Link normal, isolated;
    TextView status;
    int expectedRows=512;
    String scenario, root;
    File eventFile;
    volatile boolean active=true;
    long lastTick;
    Messenger replies;
    class Link implements ServiceConnection {
        volatile Messenger service;
        final CountDownLatch connected=new CountDownLatch(1), died=new CountDownLatch(1);
        final Class<?> type;
        Link(Class<?> c) { type=c; }
        public void onServiceConnected(ComponentName name,IBinder binder) {
            service=new Messenger(binder);
            try { binder.linkToDeath(()->{ event("binder_death",obj("component",type.getSimpleName())); died.countDown(); },0); }
            catch(RemoteException e) { event("bind_death_error",obj("error",e.toString())); }
            event("connected",obj("component",type.getSimpleName())); connected.countDown();
        }
        public void onServiceDisconnected(ComponentName n) { event("disconnected",obj("component",type.getSimpleName())); service=null; died.countDown(); }
        public void onBindingDied(ComponentName n) { event("binding_died",obj("component",type.getSimpleName())); died.countDown(); }
    }
    static JSONObject obj(Object... pairs) {
        JSONObject o=new JSONObject();
        try { for(int i=0;i<pairs.length;i+=2)o.put((String)pairs[i],pairs[i+1]); }
        catch(JSONException e) { throw new RuntimeException(e); }
        return o;
    }
    synchronized void event(String name,JSONObject data) {
        JSONObject row=obj("event",name,"elapsed_ms",SystemClock.elapsedRealtime(),"host_pid",Process.myPid(),"host_uid",Process.myUid(),"data",data);
        try(FileOutputStream out=new FileOutputStream(eventFile,true)) {
            out.write((row.toString()+"\n").getBytes(StandardCharsets.UTF_8)); out.getFD().sync();
        } catch(IOException e) { throw new RuntimeException(e); }
        ui.post(()->status.setText("journal runtime development\n"+scenario+"\n"+name+"\n"+data.toString()));
        if(name.equals("complete")||name.equals("failed")||name.equals("host_killed_during_work")||name.equals("host_intentional_kill")||name.equals("host_finish")) android.util.Log.i("JournalRuntime","RUNTIME_DONE:"+eventFile.getName());
    }
    void check(boolean pass,String message) { if(!pass)throw new IllegalStateException(message); }
    Link bind(Class<?> c) throws Exception {
        Link l=new Link(c); CountDownLatch invoked=new CountDownLatch(1);
        ui.post(()->{check(bindService(new Intent(this,c),l,Context.BIND_AUTO_CREATE),"bind refused"); invoked.countDown();});
        check(invoked.await(10,TimeUnit.SECONDS),"bind invocation timeout");
        check(l.connected.await(20,TimeUnit.SECONDS),"bind connection timeout"); return l;
    }
    void unbind(Link l) throws Exception {
        CountDownLatch done=new CountDownLatch(1);
        ui.post(()->{unbindService(l);done.countDown();}); check(done.await(10,TimeUnit.SECONDS),"unbind timeout");
    }
    CompletableFuture<JSONObject> send(Link l,String op,int count) throws Exception {
        int id=next.incrementAndGet(); CompletableFuture<JSONObject> future=new CompletableFuture<>(); pending.put(id,future);
        Message m=Message.obtain(null,1);m.arg1=id;m.replyTo=replies;
        Bundle b=new Bundle(); b.putString("op",op);b.putString("root",root);b.putInt("count",count);m.setData(b);
        try { check(l.service!=null,"service disconnected"); l.service.send(m); }
        catch(Exception e) { pending.remove(id); throw e; }
        future.whenComplete((v,e)->pending.remove(id)); return future;
    }
    JSONObject call(Link l,String op,int count) throws Exception {
        long start=SystemClock.elapsedRealtimeNanos(); CompletableFuture<JSONObject> f=send(l,op,count);
        JSONObject result;
        try { result=f.get(90,TimeUnit.SECONDS); }
        catch(Exception e) { f.completeExceptionally(e); throw e; }
        result.put("roundtrip_ms",(SystemClock.elapsedRealtimeNanos()-start)/1e6);
        event(op,result); return result;
    }
    JSONObject value(JSONObject result) throws Exception { return result.getJSONObject("native").getJSONObject("value"); }
    void requireWork(JSONObject result) throws Exception {
        check(result.getJSONObject("native").getBoolean("ok"),"native work failed: "+result);
        JSONObject v=value(result); check(v.getString("sqlite_check").equals("ok"),"SQLite check");
        check(v.getJSONObject("result").getLong("total")==expectedRows,"expected exact fixture population");
        check(v.getJSONObject("result").getJSONArray("results").length()==8,"bounded query rows");
    }
    void seed() throws Exception {
        File f=new File(root,"facets/work/activities/20261011.jsonl");
        if(f.exists())return;
        check(f.getParentFile().mkdirs()||f.getParentFile().isDirectory(),"fixture mkdir");
        try(FileOutputStream out=new FileOutputStream(f)) {
            for(int i=0;i<512;i++) {
                JSONObject record=obj("id","synthetic-"+i,"title","Synthetic Android evidence "+i,"story",obj("body","androidsentinel persistent query result uniqueentry"+i));
                out.write((record.toString()+"\n").getBytes(StandardCharsets.UTF_8));
            }
            out.getFD().sync();
        }
        event("fixture",obj("rows",512,"path",f.toString(),"bytes",f.length()));
    }
    void crash(String op) throws Exception {
        Link old=normal; JSONObject before=call(old,"identity",1);
        CompletableFuture<JSONObject> lost=send(old,op,1);
        check(old.died.await(20,TimeUnit.SECONDS),"worker did not disconnect");
        lost.completeExceptionally(new EOFException("worker died"));
        event("worker_failure_observed",obj("operation",op,"old_pid",before.getInt("pid"),"host_survived",true));
        unbind(old);normal=bind(JournalService.class);
        JSONObject after=call(normal,"identity",1);
        check(before.getInt("pid")!=after.getInt("pid"),"worker PID did not change");
        JSONObject recovery=value(call(normal,"recovery",1));
        check(recovery.getString("checkpoint").equals("{\"committed\":true}"),"interrupted publication changed committed checkpoint");
        check(recovery.getInt("uncommitted_ghost_rows")==0,"uncommitted SQLite edit survived");
        requireWork(call(normal,"work",1));
    }
    void suite() {
        try {
            event("start",obj("scenario",scenario,"os",Build.VERSION.RELEASE,"sdk",Build.VERSION.SDK_INT,"fingerprint",Build.FINGERPRINT,"root",root));
            if(!scenario.equals("mixed"))seed(); normal=bind(JournalService.class);
            JSONObject identity=call(normal,"identity",1);
            check(identity.getInt("pid")!=Process.myPid(),"worker must be separate process");
            check(identity.getInt("uid")==Process.myUid(),"ordinary worker must share app authority");
            if(scenario.equals("mixed")) { mixed(); return; }
            JSONObject initial=call(normal,"work",1);
            if(scenario.equals("baseline")) { event("complete",obj("baseline_native_ok",initial.getJSONObject("native").getBoolean("ok"))); return; }
            requireWork(initial);
            if(scenario.equals("probe")) { JSONObject result=call(normal,"publication",1); event("complete",obj("publication_native_ok",result.getJSONObject("native").getBoolean("ok"))); return; }
            if(scenario.equals("verify")) { check(value(call(normal,"snapshot",1)).getBoolean("checkpointed"),"snapshot failed"); event("complete",obj("persisted_query",true)); return; }
            if(scenario.equals("exit")) { event("host_intentional_kill",obj("pid",Process.myPid())); Process.killProcess(Process.myPid()); return; }
            if(scenario.equals("loss")) {
                call(normal,"begin",1);send(normal,"repeat",100000);Thread.sleep(300);
                event("host_killed_during_work",obj("pid",Process.myPid()));Process.killProcess(Process.myPid());return;
            }
            if(scenario.equals("finish")) { event("host_finish",obj("pid",Process.myPid()));ui.post(()->finishAndRemoveTask());return; }
            JSONObject authority=value(call(normal,"authority",1));
            for(String key:new String[]{"reopen_identity","root_replacement_refused","bound_original_retained","symlink_refused","overwrite_refused","lock_held","lock_released"}) check(authority.getBoolean(key),"authority test failed: "+key);
            check(value(call(normal,"publication",1)).getBoolean("confirmed"),"publication must be confirmed");
            check(value(call(normal,"publication-race",1)).getInt("winners")==1,"publication race failed");
            check(value(call(normal,"publication-faults",1)).getBoolean("passed"),"publication fault states failed");
            isolated=bind(IsolatedService.class);
            JSONObject isolatedId=call(isolated,"identity",1);
            check(isolatedId.getInt("uid")!=Process.myUid(),"isolated worker UID");
            JSONObject isolatedWork=call(isolated,"work",1);
            check(!isolatedWork.getJSONObject("native").getBoolean("ok"),"isolated worker unexpectedly has private journal path access");
            unbind(isolated); isolated=null;
            crash("crash");
            crash("transaction-crash");
            crash("atomic-crash");
            crash("exclusive-crash-before");
            check(!new File(root,"exclusive-before").exists(),"interrupted create became visible");
            crash("exclusive-crash-after");
            check(new String(java.nio.file.Files.readAllBytes(new File(root,"exclusive-after").toPath()),StandardCharsets.UTF_8).equals("complete exclusive crash fixture"),"committed create lost after reply loss");
            call(normal,"begin",1);
            CompletableFuture<JSONObject> repeated=send(normal,"repeat",100000);
            Thread.sleep(250);
            long cancelStart=SystemClock.elapsedRealtimeNanos();call(normal,"cancel",1);
            JSONObject cancelled=repeated.get(30,TimeUnit.SECONDS);
            event("cancelled_batch",obj("response",cancelled,"cancellation_ms",(SystemClock.elapsedRealtimeNanos()-cancelStart)/1e6));
            check(value(cancelled).getBoolean("cancelled"),"batch must cancel");
            call(normal,"begin",1);requireWork(call(normal,"work",1));
            long start=SystemClock.elapsedRealtime();
            for(int i=0;i<120;i++) { requireWork(call(normal,"work",1)); Thread.sleep(1000); }
            event("repeated_use",obj("requests",120,"elapsed_ms",SystemClock.elapsedRealtime()-start));
            check(value(call(normal,"snapshot",1)).getBoolean("checkpointed"),"snapshot failed");
            synchronized(ticks) { event("ui_ticks",obj("nominal_interval_ms",50,"gaps_ms",new JSONArray(ticks))); }
            event("complete",obj("passed",true,"pending",pending.size()));
        } catch(Throwable e) { event("failed",obj("error",e.toString(),"stack",android.util.Log.getStackTraceString(e))); }
    }
    void mixed() throws Exception {
        expectedRows=4096;
        requireWork(call(normal,"seed-mixed",1));
        call(normal,"resources",1);
        long start=SystemClock.elapsedRealtime();
        // 30 minutes: repeated connection use, source updates and index convergence.
        for(int i=1;i<=360;i++) {
            requireWork(call(normal,i%10==0?"mutate":i%5==0?"work":"query",i));
            if(i%10==0)call(normal,"resources",i);
            Thread.sleep(5000);
        }
        check(value(call(normal,"snapshot",1)).getBoolean("checkpointed"),"snapshot failed");call(normal,"resources",1);
        synchronized(ticks) { event("ui_ticks",obj("nominal_interval_ms",50,"gaps_ms",new JSONArray(ticks))); }
        event("repeated_use",obj("requests",360,"elapsed_ms",SystemClock.elapsedRealtime()-start,"rows",4096));
        event("complete",obj("passed",true,"pending",pending.size()));
    }
    public void onCreate(Bundle b) {
        super.onCreate(b);getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        status=new TextView(this);status.setTextSize(16);
        LinearLayout controls=new LinearLayout(this);controls.setOrientation(LinearLayout.VERTICAL);
        Button cancel=new Button(this);cancel.setText("cancel batch");cancel.setOnClickListener(v->{
            new Thread(()->{try { if(normal!=null)call(normal,"cancel",1); }catch(Exception e){event("control_error",obj("error",e.toString()));}},"cancel-control").start();
        });
        controls.addView(cancel);controls.addView(status);setContentView(controls);
        scenario=getIntent().getStringExtra("scenario");if(scenario==null)scenario="full";
        root=new File(getFilesDir(),scenario.equals("mixed")?"synthetic-journal-mixed":"synthetic-journal").toString();
        String runId=getIntent().getStringExtra("run_id");if(runId==null)runId=Long.toString(System.currentTimeMillis());
        eventFile=new File(getFilesDir(),"events-"+scenario+"-"+runId+".jsonl");
        replies=new Messenger(new Handler(Looper.getMainLooper()) {
            public void handleMessage(Message m) {
                Bundle d=m.getData();
                JSONObject result=obj("native",parse(d.getString("result")),"pid",d.getInt("pid"),"uid",d.getInt("uid"),"native_ms",d.getLong("nanos")/1e6,"pss_kb",d.getInt("pss_kb"),"threads",d.getInt("threads"),"java_heap",d.getLong("java_heap"));
                CompletableFuture<JSONObject> f=pending.get(m.arg1);if(f!=null)f.complete(result);
            }
        });
        lastTick=SystemClock.elapsedRealtime();ui.postDelayed(new Runnable(){public void run(){long now=SystemClock.elapsedRealtime();synchronized(ticks){ticks.add(now-lastTick);}lastTick=now;if(active)ui.postDelayed(this,50);}},50);
        new Thread(this::suite,"runtime-driver").start();
    }
    static JSONObject parse(String s) {try{return new JSONObject(s);}catch(JSONException e){throw new RuntimeException(e);} }
    protected void onDestroy() {active=false;if(normal!=null)unbindService(normal);if(isolated!=null)unbindService(isolated);super.onDestroy();}
}
