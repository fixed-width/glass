using System;
using System.Collections.Generic;

internal static class ChildOwnershipTests {
    static Journal Pending() { return new Journal {parent=new ProcessRecord {session=2},child_pending=true}; }
    static void Require(bool value,string message) { if (!value) throw new InvalidOperationException(message); }
    static void Refuses(Action action) {
        try { action(); } catch (InvalidOperationException) { return; }
        throw new Exception("Unverified cleanup was accepted");
    }
    internal static void Run(List<string> passed) {
        var state=Pending(); var saved=new List<string>();
        ChildOwnership.LoginCompleted(state,()=>saved.Add(state.child+":"+state.child_logon+":"+state.child_pending),()=>7,id=>101);
        Require(saved.Count==2 && saved[0]==UInt32.MaxValue+":0:True" && saved[1]=="7:101:False","Login did not persist pending ownership before its exact generation");
        // Cancellation or parent exit can happen before the first timer tick.
        ChildOwnership.RequireResolved(state);
        Require(state.child==7 && state.child_logon==101 && !state.child_gone,"Immediate stop lost the authenticated child");
        passed.Add("login records child before cancellation or parent exit");

        state=Pending();
        ChildOwnership.LoginCompleted(state,()=>{},()=>UInt32.MaxValue,id=>{throw new Exception("No child to query");});
        Refuses(()=>ChildOwnership.RequireResolved(state));
        Require(!state.child_gone && state.child_pending,"Missing child was reported gone");
        passed.Add("unresolved child retains cleanup ownership");

        state=Pending();
        Require(!ChildOwnership.Capture(state,()=>7,id=>0),"Incomplete generation was accepted");
        Require(state.child==UInt32.MaxValue && state.child_logon==0,"Partial identity was published");
        Refuses(()=>ChildOwnership.RequireResolved(state));
        Require(ChildOwnership.Capture(state,()=>7,id=>101),"Later identity did not resolve");
        ChildOwnership.RequireResolved(state);
        passed.Add("delayed generation can resolve before teardown");

        Refuses(()=>ChildOwnership.Capture(state,()=>7,id=>202));
        Require(state.child==7 && state.child_logon==101,"Reused session replaced owned generation");
        passed.Add("changed child generation is never adopted");

        state=Pending();
        Refuses(()=>ChildOwnership.Capture(state,()=>2,id=>101));
        Refuses(()=>ChildOwnership.Capture(state,()=>0,id=>101));
        Require(state.child==UInt32.MaxValue && state.child_pending,"Parent or system session was adopted");
        passed.Add("parent and system sessions cannot become child ownership");

        Refuses(()=>ChildOwnership.LoginCompleted(state,()=>{},()=>{throw new InvalidOperationException("query failed");},id=>101));
        Require(state.child_pending && !state.child_gone,"Query failure discarded pending ownership");
        passed.Add("failed native identity query remains incomplete");
    }
}
