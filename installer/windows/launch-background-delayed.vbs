Set sh = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
install = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\bin")
If Not fso.FolderExists(install) Then install = fso.GetParentFolderName(WScript.ScriptFullName)
target = install & "\launch-background.vbs"
If Not fso.FileExists(target) Then WScript.Quit 0
WScript.Sleep 45000
sh.Run "wscript.exe //nologo """ & target & """", 0, False
