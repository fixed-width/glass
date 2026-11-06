using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Security.AccessControl;
using System.Security.Principal;
using System.Text.RegularExpressions;
using System.Web.Script.Serialization;

// Build alongside Host.cs, Tray.cs and Startup.cs with /main:TaskAccessTest.
public sealed class RestrictedScheduler {
    readonly dynamic service;
    readonly string folder;
    public RestrictedScheduler(string folder) {
        this.folder=folder;
        service=Activator.CreateInstance(Type.GetTypeFromProgID("Schedule.Service",true));
        service.Connect();
    }
    public dynamic GetFolder(string path) {
        if(path!="\\")throw new InvalidOperationException("Unexpected task-folder request");
        return service.GetFolder(folder);
    }
    public dynamic NewTask(int flags) { return service.NewTask(flags); }
}

internal static class TaskAccessTest {
    [DllImport("advapi32.dll",SetLastError=true)]static extern bool GetTokenInformation(IntPtr token,int kind,out int value,int length,out int needed);
    static void Require(bool condition,string detail) { if(!condition)throw new InvalidOperationException(detail); }
    static bool Equal(byte[] left,byte[] right) {
        if(left.Length!=right.Length)return false;
        for(int i=0;i<left.Length;i++)if(left[i]!=right[i])return false;
        return true;
    }
    [MTAThread]static int Main(string[] args) {
        var result=new Dictionary<string,object>();bool passed=false;
        try {
            string nonce=args[0],folderPath=args[1],reports=Path.GetFullPath(args[2]);
            Require(Regex.IsMatch(nonce,"^[a-f0-9]{6,32}$"),"Invalid fixture nonce");
            Require(folderPath=="\\Glass.Policy."+nonce,"Task folder is outside fixture scope");
            var identity=WindowsIdentity.GetCurrent();
            foreach(var group in identity.Groups)Require(group.Value!="S-1-5-32-544","Test requires a genuine standard account");
            int elevation,needed;
            Require(GetTokenInformation(identity.Token,18,out elevation,4,out needed) && elevation==1,"Test requires a default standard token");
            result["token"]=new {sid=identity.User.Value,elevation_type=elevation,administrator_sid_present=false,session=Process.GetCurrentProcess().SessionId};
            uint childBefore=Native.Child();bool enabledBefore;
            Require(Native.WTSIsChildSessionsEnabled(out enabledBefore),"Cannot read native child setting");
            var scheduler=new RestrictedScheduler(folderPath);dynamic folder=scheduler.GetFolder("\\");
            string before=folder.GetSecurityDescriptor(7);int beforeCount=folder.GetTasks(0).Count;
            Require(beforeCount==0,"Fixture task folder must be empty");
            string root=Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),"GlassTaskPolicyFixture-"+nonce);
            Require(!Directory.Exists(root),"Private fixture directory already exists");
            Directory.CreateDirectory(root);
            var security=new DirectorySecurity();security.SetOwner(identity.User);security.SetAccessRuleProtection(true,false);
            foreach(string sid in new[]{"S-1-3-4","S-1-5-18"})security.AddAccessRule(new FileSystemAccessRule(new SecurityIdentifier(sid),FileSystemRights.FullControl,InheritanceFlags.ContainerInherit|InheritanceFlags.ObjectInherit,PropagationFlags.None,AccessControlType.Allow));
            Directory.SetAccessControl(root,security);Files.RequirePrivate(root);
            string executable=Process.GetCurrentProcess().MainModule.FileName;
            string request=Guid.NewGuid().ToString("N");
            var recipe=new StartupRecipe {version=1,sid=identity.User.Value,enabled=true,request=request,executable=executable,
                arguments=new[]{"serve","--tray","--startup","--addr","127.0.0.1:7300"},helpers=new[]{executable}};
            Files.WriteText(root,"startup.json",new JavaScriptSerializer().Serialize(recipe));
            byte[] original=File.ReadAllBytes(Path.Combine(root,"startup.json"));
            int code=Startup.Configure(root,false,()=>scheduler);
            var receipt=Files.Read<Dictionary<string,object>>(root,"startup-receipt.json");
            string error=(string)receipt["error"];
            Require(code==1 && !(bool)receipt["enabled"],"Denied registration was reported successful");
            Require(error!=null && error.IndexOf("0x80070005",StringComparison.OrdinalIgnoreCase)>=0,"Native access-denied diagnostic was lost");
            Require(error.Contains("register Glass startup") && error.Contains("administrator"),"Denial lacks operation or administrator guidance");
            Require((string)receipt["request"]==request,"Receipt belongs to another request");
            Require(Equal(original,File.ReadAllBytes(Path.Combine(root,"startup.json"))),"Failed operation rewrote the recipe");
            string after=folder.GetSecurityDescriptor(7);int afterCount=folder.GetTasks(0).Count;
            Require(before==after && afterCount==0,"Denied operation changed the task folder");
            bool enabledAfter;
            Require(Native.WTSIsChildSessionsEnabled(out enabledAfter) && enabledBefore==enabledAfter && Native.Child()==childBefore,"Task denial changed child-session state");
            string worker=Startup.TaskFailure("start the child-session worker through Task Scheduler",new COMException("Injected access denial",unchecked((int)0x80070005)));
            Require(worker.Contains("child-session worker") && worker.Contains("administrator") && worker.Contains("0x80070005"),"Worker denial lost its context");
            result["receipt"]=receipt;result["registration_exit_code"]=code;result["folder_unchanged"]=true;
            result["folder_descriptor"]=before;result["tasks_before"]=beforeCount;result["tasks_after"]=afterCount;
            result["native_setting_before"]=enabledBefore;result["native_setting_after"]=enabledAfter;result["child_unchanged"]=true;
            result["worker_message_injected_error"]=worker;result["private_state_owner"]=identity.User.Value;
            File.WriteAllBytes(Path.Combine(reports,"startup-recipe.json"),original);
            File.WriteAllBytes(Path.Combine(reports,"startup-error-receipt.json"),File.ReadAllBytes(Path.Combine(root,"startup-receipt.json")));
            Directory.Delete(root,true);result["private_root_removed"]=!Directory.Exists(root);
            passed=true;
        }catch(Exception error){result["error"]=error.ToString();}
        result["passed"]=passed;result["utc"]=DateTime.UtcNow.ToString("o");
        File.WriteAllText(Path.Combine(Path.GetFullPath(args[2]),"task-denied-result.json"),new JavaScriptSerializer().Serialize(result));
        return passed ? 0 : 1;
    }
}
