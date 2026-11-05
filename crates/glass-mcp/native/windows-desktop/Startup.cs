using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Security.Cryptography;
using System.Security.Principal;
using System.Text;
using System.Threading;
using System.Windows.Forms;

public sealed class StartupRecipe {
    public uint version;
    public string sid,executable,request;
    public bool enabled;
    public string[] arguments,helpers;
}

internal static class Startup {
    internal static string TaskFailure(string operation,Exception failure) {
        string guidance=failure is UnauthorizedAccessException || unchecked((uint)failure.HResult)==0x80070005
            ? " Ask your administrator to review Task Scheduler permissions." : "";
        return "Windows could not "+operation+" (HRESULT 0x"+unchecked((uint)failure.HResult).ToString("X8")+")."+guidance+" "+failure;
    }
    static string Name(string root) {
        using(var hash=SHA256.Create())return "Glass.Startup."+BitConverter.ToString(hash.ComputeHash(Encoding.UTF8.GetBytes(root.ToLowerInvariant()))).Replace("-","").Substring(0,16).ToLowerInvariant();
    }
    static string Sid(string value) {
        return value.StartsWith("S-1-") ? value : ((SecurityIdentifier)new NTAccount(value).Translate(typeof(SecurityIdentifier))).Value;
    }
    static void Validate(StartupRecipe recipe) {
        if(recipe.version!=1 || recipe.sid!=WindowsIdentity.GetCurrent().User.Value || recipe.request==null || recipe.request.Length!=32 ||
            !Path.IsPathRooted(recipe.executable) || recipe.arguments==null || recipe.helpers==null || recipe.helpers.Length==0 || recipe.helpers.Length>64 ||
            (recipe.arguments.Length!=5 && recipe.arguments.Length!=7) || recipe.arguments[0]!="serve" || recipe.arguments[1]!="--tray" || recipe.arguments[2]!="--startup" || recipe.arguments[3]!="--addr" ||
            (recipe.arguments.Length==7 && (recipe.arguments[5]!="--token-file" || !Path.IsPathRooted(recipe.arguments[6]))))
            throw new InvalidOperationException("Windows startup settings are not a valid Glass launch");
        bool known=false;string current=Process.GetCurrentProcess().MainModule.FileName;
        foreach(string helper in recipe.helpers)if(String.Equals(helper,current,StringComparison.OrdinalIgnoreCase))known=true;
        if(!known)throw new InvalidOperationException("Windows startup bootstrap changed");
    }
    static void Owned(dynamic task,string root,StartupRecipe recipe) {
        dynamic definition=task.Definition;
        if(definition.Actions.Count!=1 || definition.Triggers.Count!=1 || Sid((string)definition.Principal.UserId)!=recipe.sid ||
            definition.Principal.LogonType!=3 || definition.Principal.RunLevel!=0)
            throw new InvalidOperationException("Windows startup registration was changed; it was retained");
        dynamic action=definition.Actions.Item(1),trigger=definition.Triggers.Item(1);
        bool known=false;foreach(string helper in recipe.helpers)if(String.Equals(helper,(string)action.Path,StringComparison.OrdinalIgnoreCase))known=true;
        if(!known || (string)action.Arguments!=Files.Quote(root)+" startup" ||
            !String.Equals((string)action.WorkingDirectory,root,StringComparison.OrdinalIgnoreCase) || trigger.Type!=9 || Sid((string)trigger.UserId)!=recipe.sid)
            throw new InvalidOperationException("Windows startup action or trigger was changed; it was retained");
    }
    static dynamic Connect() {
        dynamic service=Activator.CreateInstance(Type.GetTypeFromProgID("Schedule.Service",true));service.Connect();return service;
    }
    internal static int Configure(string root,bool inspect) { return Configure(root,inspect,Connect); }
    internal static int Configure(string root,bool inspect,Func<dynamic> connect) {
        StartupRecipe recipe=null;string taskName=Name(root);bool enabled=false;string error=null;
        string operation="read Glass startup settings";
        try {
            recipe=Files.Read<StartupRecipe>(root,"startup.json");Validate(recipe);
            operation="connect to Task Scheduler";dynamic service=connect();
            operation="inspect Glass startup registration";
            dynamic folder=service.GetFolder("\\"),existing=null;
            try{existing=folder.GetTask(taskName);}catch(FileNotFoundException){}
            catch(System.Runtime.InteropServices.COMException missing){if((uint)missing.ErrorCode!=0x80070002)throw;}
            if(existing!=null){Owned(existing,root,recipe);enabled=existing.Enabled;}
            if(!inspect) {
                if(recipe.enabled) {
                    dynamic task=service.NewTask(0);
                    task.RegistrationInfo.Description="Open Glass in the system tray when this user signs in.";
                    task.Principal.UserId=recipe.sid;task.Principal.LogonType=3;task.Principal.RunLevel=0;
                    task.Settings.ExecutionTimeLimit="PT0S";task.Settings.DisallowStartIfOnBatteries=false;task.Settings.StopIfGoingOnBatteries=false;
                    task.Settings.MultipleInstances=2;
                    dynamic trigger=task.Triggers.Create(9);trigger.UserId=recipe.sid;trigger.Enabled=true;
                    dynamic action=task.Actions.Create(0);action.Path=Process.GetCurrentProcess().MainModule.FileName;
                    action.Arguments=Files.Quote(root)+" startup";action.WorkingDirectory=root;
                    operation="register Glass startup";
                    folder.RegisterTaskDefinition(taskName,task,6,null,null,3,null);
                    dynamic registered=folder.GetTask(taskName);Owned(registered,root,recipe);enabled=registered.Enabled;
                } else {
                    operation="disable Glass startup";if(existing!=null)existing.Enabled=false;
                    enabled=false;
                }
            }
        }catch(Exception failure){
            error=TaskFailure(operation,failure);
        }
        Files.WriteText(root,"startup-receipt.json",new System.Web.Script.Serialization.JavaScriptSerializer().Serialize(
            new { version=1,request=recipe==null ? "" : recipe.request,enabled=enabled,task=taskName,error=error }));
        return error==null ? 0 : 1;
    }
    internal static int Run(string root) {
        try {
            var recipe=Files.Read<StartupRecipe>(root,"startup.json");Validate(recipe);
            if(!recipe.enabled)return 0;
            var current=ProcessRecord.Read((uint)Process.GetCurrentProcess().Id);
            if(current.session==0)throw new InvalidOperationException("Glass startup needs the signed-in Windows desktop");
            string index=Path.Combine(root,"tray.json");
            if(File.Exists(index)) {
                var live=Files.Read<Launch>(root,"tray.json");
                // Signing in to a child can trigger startup again for the same account.
                if(live.parent!=null && live.parent.Live() && live.parent.sid==current.sid)return 0;
            }
            if(!File.Exists(recipe.executable))throw new InvalidOperationException("The Glass executable has moved. Open Glass and save Start with Windows again.");
            var arguments=new List<string>();foreach(string argument in recipe.arguments)arguments.Add(Files.Quote(argument));
            var info=new ProcessStartInfo(recipe.executable,String.Join(" ",arguments.ToArray())) { UseShellExecute=false,CreateNoWindow=true,WorkingDirectory=root };
            info.EnvironmentVariables["GLASS_WINDOWS_DESKTOP_STATE_DIR"]=root;
            using(var process=Process.Start(info)) {
                DateTime deadline=DateTime.UtcNow.AddSeconds(15);
                while(DateTime.UtcNow<deadline && !process.HasExited) {
                    if(File.Exists(index)) {
                        var launch=Files.Read<Launch>(root,"tray.json");
                        if(launch.parent!=null && launch.parent.pid==(uint)process.Id && launch.parent.Live() && launch.helper!=null && launch.helper.Live()) {
                            Files.WriteText(root,"startup-result.json","{\"started\":true,\"resident_pid\":"+process.Id+"}");return 0;
                        }
                    }
                    Thread.Sleep(50);
                }
                throw new InvalidOperationException("Glass could not finish startup. Open Glass to inspect Application setup.");
            }
        }catch(Exception error){Files.WriteText(root,"startup-error.txt",error.ToString());return 1;}
    }
}
