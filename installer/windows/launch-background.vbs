Set sh = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
install = fso.GetParentFolderName(WScript.ScriptFullName)
lib = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\lib")
If Not fso.FolderExists(lib) Then lib = install & "\lib"
Set env = sh.Environment("PROCESS")
env("PATH") = install & ";" & lib & ";" & env("PATH")
sh.Run """" & install & "\run-background.cmd""", 0, False
