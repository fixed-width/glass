using System;
using System.Collections.Generic;
using System.IO;
using System.Web.Script.Serialization;

// Compile with Host.cs, Tray.cs and Startup.cs using /main:SetupFlowTests.
internal static class SetupFlowTests {
    static readonly DateTime Now=new DateTime(2026,11,6,0,0,0,DateTimeKind.Utc);
    static TrayStatus Status(string mode,bool ready,bool busy,ulong sequence){return new TrayStatus {mode=mode,ready=ready,busy=busy,acknowledged=sequence,endpoint="http://127.0.0.1:7300/"};}
    static void Require(bool value,string detail){if(!value)throw new InvalidOperationException(detail);}
    static void Test(List<string> passed,string name,Action body){body();passed.Add(name);}
    static int Main(string[] args){
        var passed=new List<string>();var report=new Dictionary<string,object>();
        try{
            ChildOwnershipTests.Run(passed);
            Test(passed,"first run requires desktop choice",()=>{
                var flow=new SetupFlow();var status=Status(null,false,false,0);flow.Observe(status);
                Require(flow.Page(status,Now)==SetupPage.Desktop,"Fresh setup skipped desktop selection");
            });
            Test(passed,"quick main check skips preparation flash",()=>{
                var flow=new SetupFlow();var before=Status(null,false,false,0);flow.Observe(before);flow.Choose(before,"shared",1,Now);
                var checking=Status("shared",false,true,1);flow.Observe(checking);
                Require(flow.Page(checking,Now.AddMilliseconds(100))==SetupPage.Desktop,"Fast check flashed preparation");
                var ready=Status("shared",true,false,1);flow.Observe(ready);
                Require(flow.Page(ready,Now.AddMilliseconds(150))==SetupPage.Connect,"Ready main desktop inserted another action");
            });
            Test(passed,"slow main check shows progress without delaying ready",()=>{
                var flow=new SetupFlow();var before=Status(null,false,false,0);flow.Observe(before);flow.Choose(before,"shared",1,Now);
                var checking=Status("shared",false,true,1);flow.Observe(checking);
                Require(flow.Page(checking,Now.AddMilliseconds(450))==SetupPage.Prepare,"Slow check did not expose progress");
                var ready=Status("shared",true,false,1);flow.Observe(ready);
                Require(flow.Page(ready,Now.AddMilliseconds(451))==SetupPage.Connect,"Readiness was held for a cosmetic timer");
            });
            Test(passed,"separate desktop waits for actual readiness",()=>{
                var flow=new SetupFlow();var status=Status("separate",false,false,0);flow.Observe(status);
                Require(flow.Page(status,Now)==SetupPage.Prepare,"Unprepared separate desktop exposed connection step");
            });
            Test(passed,"rejected choice stays editable",()=>{
                var flow=new SetupFlow();var before=Status("shared",true,false,0);flow.Observe(before);flow.Choose(before,"separate",1,Now);
                var failed=Status("shared",true,false,1);flow.Observe(failed);
                Require(flow.Page(failed,Now)==SetupPage.Desktop,"Rejected choice changed the visible selection flow");
            });
            Test(passed,"cancellation can supersede pending choice sequence",()=>{
                var flow=new SetupFlow();var before=Status(null,false,false,0);flow.Observe(before);flow.Choose(before,"shared",1,Now);
                var cancelled=Status("shared",false,false,2);flow.Observe(cancelled);
                Require(flow.Page(cancelled,Now)==SetupPage.Prepare,"Cancelled checks left a stale pending choice");
            });
            Test(passed,"copying is not setup completion",()=>{
                var flow=new SetupFlow();var ready=Status("shared",true,false,0);flow.Observe(ready);flow.Copied(ready.endpoint);
                Require(!flow.Complete(ready) && flow.Page(ready,Now)==SetupPage.Connect,"Copying claimed agent connection");
            });
            Test(passed,"stopping after finish keeps operating view",()=>{
                var flow=new SetupFlow();var ready=Status("separate",true,false,0);flow.Observe(ready);flow.Finish(ready.endpoint);
                var stopped=Status("separate",false,false,1);flow.Observe(stopped);
                Require(flow.Page(stopped,Now)==SetupPage.Operating,"Environment stop repeated agent setup");
            });
            Test(passed,"same choice retains finished environment",()=>{
                var flow=new SetupFlow();var ready=Status("separate",true,false,0);flow.Observe(ready);flow.Finish(ready.endpoint);flow.Edit();flow.Choose(ready,"separate",1,Now);
                var saved=Status("separate",true,false,1);flow.Observe(saved);
                Require(flow.Page(saved,Now)==SetupPage.Operating,"Saving the same desktop restarted onboarding");
            });
            Test(passed,"changed desktop retains endpoint but guides preparation",()=>{
                var flow=new SetupFlow();var ready=Status("shared",true,false,0);flow.Observe(ready);flow.Finish(ready.endpoint);flow.Choose(ready,"separate",1,Now);
                var saved=Status("separate",false,false,1);flow.Observe(saved);
                Require(flow.Page(saved,Now)==SetupPage.Prepare,"Changed desktop incorrectly appeared operational");
                var prepared=Status("separate",true,false,2);flow.Observe(prepared);
                Require(flow.Page(prepared,Now)==SetupPage.Operating,"Changed desktop repeated endpoint configuration");
            });
            Test(passed,"pending switch waits for the chosen desktop",()=>{
                var flow=new SetupFlow();var ready=Status("shared",true,false,0);flow.Observe(ready);flow.Finish(ready.endpoint);flow.Choose(ready,"separate",1,Now);
                var stopping=Status("shared",false,true,1);flow.Observe(stopping);
                Require(flow.Page(stopping,Now.AddMilliseconds(500))==SetupPage.Desktop,"Stopping the old desktop was mistaken for preparing the new one");
            });
            Test(passed,"different endpoint requires acknowledgement",()=>{
                var flow=new SetupFlow();var ready=Status("shared",true,false,0);flow.Observe(ready);flow.Finish(ready.endpoint);
                var changed=Status("shared",true,false,0);changed.endpoint="http://127.0.0.1:7400/";flow.Observe(changed);
                Require(flow.Page(changed,Now)==SetupPage.Connect,"Old endpoint acknowledgement applied to a different address");
            });
            Test(passed,"startup draft is retained until explicit save",()=>{
                var flow=new SetupFlow();var before=Status(null,false,false,0);flow.Observe(before);
                var ready=Status("shared",true,false,1);ready.start_with_windows=false;flow.Observe(ready);
                Require(flow.StartupDraft,"Fresh startup preference was overwritten before user review");
            });
            report["passed"]=true;
        }catch(Exception error){report["passed"]=false;report["error"]=error.ToString();}
        report["cases"]=passed;report["count"]=passed.Count;
        string json=new JavaScriptSerializer().Serialize(report);Console.WriteLine(json);
        if(args.Length>0)File.WriteAllText(args[0],json);
        return (bool)report["passed"] ? 0 : 1;
    }
}
