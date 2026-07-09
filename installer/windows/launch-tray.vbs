Set sh = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
install = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\bin")
If Not fso.FolderExists(install) Then install = fso.GetParentFolderName(WScript.ScriptFullName)
lib = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\lib")
If Not fso.FolderExists(lib) Then lib = install & "\lib"
Set env = sh.Environment("PROCESS")
env("SCALATTICE_TRAY_HIDDEN") = "1"
env("PATH") = install & ";" & lib & ";" & env("PATH")
sh.Run """" & install & "\scalattice-agent.exe"" tray", 0, False
