import subprocess as sp
from time import sleep
sp.run(["cargo","build"])

h1 = sp.Popen(["cargo","run","--", "-c","shadowquic/config_examples/server.yaml"],
                     start_new_session=True )
h2 = sp.Popen(["cargo","run","--", "-c","shadowquic/config_examples/client.yaml"],
                     start_new_session=True )

sleep(0.5)
out = sp.run(["curl",
               "http://echo.free.beeceptor.com",
                "-v",
                "--trace-time",
                "--socks5",
                "127.0.0.1:1089"],
             capture_output=True,
             text=True)


h1.terminate()
h2.terminate()
print(out.stderr)

print(out.stdout)
assert("GET" in out.stdout)