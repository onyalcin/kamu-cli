/*
 * Copyright (c) 2018 kamu.dev
 *
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/.
 */

package dev.kamu.cli.external

import java.awt.Desktop
import java.net.URI
import java.nio.file.Paths

import dev.kamu.core.utils.fs._
import dev.kamu.core.utils.{
  DockerClient,
  DockerProcess,
  DockerProcessBuilder,
  DockerRunArgs,
  OS
}
import org.apache.logging.log4j.LogManager

import scala.sys.process.{ProcessBuilder, ProcessIO}

class JupyterDockerProcessBuilder(
  dockerClient: DockerClient,
  network: String,
  environmentVars: Map[String, String]
) extends DockerProcessBuilder(
      dockerClient = dockerClient,
      id = "jupyter",
      runArgs = DockerRunArgs(
        image = DockerImages.JUPYTER,
        containerName = Some("kamu-jupyter"),
        hostname = Some("kamu-jupyter"),
        network = Some(network),
        exposePorts = List(80),
        volumeMap =
          Map(Paths.get("").toAbsolutePath -> Paths.get("/opt/workdir")),
        environmentVars = environmentVars
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
    if (!OS.isWindows) {
      logger.debug("Fixing file ownership")

      val shellCommand = Seq(
        "chown",
        "-R",
        s"${OS.uid}:${OS.gid}",
        "/opt/workdir"
      )

      dockerClient.runShell(
        DockerRunArgs(
          image = runArgs.image,
          volumeMap =
            Map(Paths.get("").toAbsolutePath -> Paths.get("/opt/workdir"))
        ),
        shellCommand
      )
    }
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
    if (!Desktop.isDesktopSupported ||
        !Desktop.getDesktop.isSupported(Desktop.Action.BROWSE))
      return

    val browserOpenerThread = new Thread {
      override def run(): Unit = {
        val token = waitForToken()

        val hostPort = getHostPort(80).get
        val uri = URI.create(
          s"http://${dockerClient.getDockerHost}:$hostPort/?token=$token"
        )

        logger.info(s"Opening in browser: $uri")
        Desktop.getDesktop.browse(uri)
      }
    }

    browserOpenerThread.setDaemon(true)
    browserOpenerThread.start()

  }
}
