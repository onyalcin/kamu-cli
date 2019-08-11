package dev.kamu.cli.external

import java.awt.Desktop
import java.net.URI

import org.apache.hadoop.fs.{FileSystem, Path}
import org.apache.log4j.LogManager

import scala.sys.process.{ProcessBuilder, ProcessIO}

class JupyterDockerProcessBuilder(
  fileSystem: FileSystem,
  network: String
) extends DockerProcessBuilder(
      dockerClient = new DockerClient(),
      id = "jupyter",
      runArgs = DockerRunArgs(
        image = "kamu/jupyter:0.0.1",
        containerName = Some("kamu-jupyter"),
        hostname = Some("kamu-jupyter"),
        network = Some(network),
        exposePorts = List(80),
        volumeMap = Map(
          fileSystem.getWorkingDirectory -> new Path("/opt/workdir")
        ),
        environmentVars = Seq("MAPBOX_ACCESS_TOKEN")
          .filter(sys.env.contains)
          .map(n => (n, sys.env(n)))
          .toMap
      )
    ) {

  override def run(
    processIO: Option[ProcessIO] = None
  ): JupyterDockerProcess = {
    val processBuilder = dockerClient.prepare(cmd)
    new JupyterDockerProcess(
      id,
      dockerClient,
      runArgs.containerName.get,
      processBuilder,
      runArgs
    )
  }

  // TODO: avoid this by setting up correct user inside the container
  def chown(): Unit = {
    logger.debug("Fixing file ownership")

    val unix = new com.sun.security.auth.module.UnixSystem()
    val shellCommand = Seq(
      "chown",
      "-R",
      s"${unix.getUid}:${unix.getGid}",
      "/opt/workdir"
    )

    dockerClient.runShell(
      DockerRunArgs(
        image = runArgs.image,
        volumeMap =
          Map(fileSystem.getWorkingDirectory -> new Path("/opt/workdir"))
      ),
      shellCommand
    )
  }
}

class JupyterDockerProcess(
  id: String,
  dockerClient: DockerClient,
  containerName: String,
  processBuilder: ProcessBuilder,
  runArgs: DockerRunArgs
) extends DockerProcess(
      "jupyter",
      dockerClient,
      containerName,
      processBuilder,
      runArgs
    ) {
  protected val logger = LogManager.getLogger(getClass.getName)

  private var token: String = ""

  def waitForToken(): String = {
    synchronized {
      while (token.isEmpty) {
        wait()
      }
      token
    }
  }

  protected override def getIOHandler(): ProcessIO = {
    val tokenRegex = raw"token=([a-z0-9]+)".r

    new ProcessIO(
      _ => (),
      stdout =>
        scala.io.Source
          .fromInputStream(stdout)
          .getLines()
          .foreach(line => System.out.println("[jupyter] " + line)),
      stderr =>
        scala.io.Source
          .fromInputStream(stderr)
          .getLines()
          .foreach(line => {
            synchronized {
              if (token.isEmpty) {
                val tokenValue = tokenRegex
                  .findFirstMatchIn(line)
                  .map(m => m.group(1))
                  .getOrElse("")

                if (tokenValue.nonEmpty) {
                  token = tokenValue
                  logger.debug(s"Got Jupyter token: $token")
                  this.notifyAll()
                }
              }
            }
            System.err.println("[jupyter] " + line)
          })
    )
  }

  def openBrowserWhenReady(): Unit = {
    if (Desktop.isDesktopSupported && Desktop.getDesktop.isSupported(
          Desktop.Action.BROWSE
        )) {
      val browserOpenerThread = new Thread {
        override def run(): Unit = {
          val token = waitForToken()

          val hostPort = getHostPort(80).get
          val uri = URI.create(s"http://localhost:$hostPort/?token=$token")

          logger.info(s"Opening in browser: $uri")
          Desktop.getDesktop.browse(uri)
        }
      }

      browserOpenerThread.setDaemon(true)
      browserOpenerThread.start()
    }
  }
}