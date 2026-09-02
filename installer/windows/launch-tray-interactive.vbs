Set sh = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
install = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\bin")
If Not fso.FolderExists(install) Then install = fso.GetParentFolderName(WScript.ScriptFullName)
lib = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\lib")
If Not fso.FolderExists(lib) Then lib = install & "\lib"
If Not CudaRuntimeOk(fso, lib, install) Then
  LogCudaMissing sh, fso, lib
  MsgBox "Scalattice Agent cannot start because the CUDA 12 runtime is missing." & vbCrLf & vbCrLf & _
    "Expected under:" & vbCrLf & "  " & lib & vbCrLf & vbCrLf & _
    "Reinstall Scalattice Agent from https://scalattice.cloud", _
    vbCritical, "Scalattice Agent"
  WScript.Quit 1
End If
Set env = sh.Environment("PROCESS")
env("SCALATTICE_TRAY_OPEN") = "1"
env("SCALATTICE_TRAY") = "1"
env("PATH") = install & ";" & lib & ";" & env("PATH")
sh.CurrentDirectory = install
sh.Run """" & install & "\scalattice-agent.exe"" tray --open", 0, False

Function CudaRuntimeOk(fso, lib, install)
  Dim names, i, name
  names = Array("cudart64_12.dll", "cublas64_12.dll", "cublasLt64_12.dll")
  For i = 0 To UBound(names)
    name = names(i)
    If Not fso.FileExists(lib & "\" & name) And Not fso.FileExists(install & "\" & name) Then
      CudaRuntimeOk = False
      Exit Function
    End If
  Next
  CudaRuntimeOk = True
End Function

Sub LogCudaMissing(sh, fso, lib)
  Dim logDir, logPath, ts, stream
  On Error Resume Next
  logDir = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\logs")
  If Not fso.FolderExists(logDir) Then fso.CreateFolder logDir
  logPath = logDir & "\agent.log"
  ts = Now
  Set stream = fso.OpenTextFile(logPath, 8, True)
  stream.WriteLine "[" & ts & "] CUDA runtime missing under " & lib & ": reinstall Scalattice Agent"
  stream.Close
  On Error Goto 0
End Sub
