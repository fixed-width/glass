$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$source = $env:GLASS_DESKTOP_SOURCE_DIR
$output = $env:GLASS_DESKTOP_BUILD_DIR
if (!$source -or !$output) { throw 'Desktop helper build paths are missing' }
New-Item -ItemType Directory -Force $output | Out-Null
Set-Location $output
Add-Type @'
using System;
using System.Reflection;
using System.Reflection.Emit;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;
public sealed class DesktopImportSink : ITypeLibImporterNotifySink {
    public void ReportEvent(ImporterEventKind kind, int code, string message) {
        if (kind == ImporterEventKind.ERROR_REFTOINVALIDTYPELIB) throw new InvalidOperationException(message);
    }
    public Assembly ResolveRef(object library) { throw new NotSupportedException("Unexpected RDP type library dependency"); }
}
public static class DesktopInteropBuild {
    [DllImport("oleaut32.dll", CharSet=CharSet.Unicode, PreserveSig=false)]
    static extern void LoadTypeLibEx(string path, int registration, out ITypeLib library);
    public static void Run(string input, string output) {
        ITypeLib library;
        LoadTypeLibEx(input, 2, out library);
        var converter = new TypeLibConverter();
        var assembly = converter.ConvertTypeLibToAssembly(library, output, TypeLibImporterFlags.None,
            new DesktopImportSink(), null, null, "GlassDesktopRdp", new Version(1, 0));
        assembly.Save(System.IO.Path.GetFileName(output));
    }
}
'@
[DesktopInteropBuild]::Run((Join-Path $env:SystemRoot 'System32\mstscax.dll'), (Join-Path $output 'GlassDesktopRdp.dll'))
$compiler = Join-Path $env:SystemRoot 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
& $compiler /nologo /target:winexe /platform:x64 /optimize+ /out:GlassDesktopHost.exe /reference:GlassDesktopRdp.dll /reference:System.Windows.Forms.dll /reference:System.Drawing.dll /reference:System.Web.Extensions.dll /reference:Microsoft.CSharp.dll (Join-Path $source 'Host.cs') (Join-Path $source 'Tray.cs') (Join-Path $source 'Startup.cs')
if ($LASTEXITCODE -ne 0) { throw 'Desktop host compilation failed' }
