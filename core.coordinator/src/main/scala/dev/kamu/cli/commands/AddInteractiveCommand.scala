/*
 * Copyright (c) 2018 kamu.dev
 *
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/.
 */

package dev.kamu.cli.commands

import java.nio.file.Paths

import pureconfig.generic.auto._
import dev.kamu.core.manifests.parsing.pureconfig.yaml
import yaml.defaults._
import dev.kamu.cli.metadata.{MetadataRepository, ResourceLoader}
import dev.kamu.core.manifests._
import dev.kamu.core.utils.fs._
import org.apache.logging.log4j.LogManager

import scala.io.StdIn

class InvalidInputException(msg: String) extends Exception(msg)

class AddInteractiveCommand(
  metadataRepository: MetadataRepository
) extends Command {
  private val logger = LogManager.getLogger(getClass.getName)

  def run(): Unit = {
    val dataset = runDatasetWizard()

    if (inputYesNo(
          "Add dataset",
          "Would you like to add this dataset to the workspace? " +
            "Otherwise it will be saved as a file in current directory.",
          true
        )) {
      metadataRepository.addDataset(dataset)
      logger.info("Added dataset")
    } else {
      val path = Paths.get("./" + dataset.id + ".yaml")
      new ResourceLoader().saveResourceToFile(dataset, path)
      logger.info(s"Saved dataset to: ${path.toAbsolutePath}")
    }
  }

  def runDatasetWizard(): DatasetSnapshot = {
    val id = input(
      "Dataset ID",
      "Specify the ID of the new dataset.\nIt is recommended that you use dot-separated " +
        "reverse domain name notation, specifying the domain where authoritative source " +
        "of the data is located followed by unique name of the dataset.\n" +
        "Example: ca.vancouver.data.property-tax-report.2018"
    )(DatasetID)

    inputChoice(
      "Kind",
      "There are two kinds of datasets. Root dataset ingests data from some " +
        "external source, like file or a resource on the web. Derivative dataset is created purely " +
        "from existing root or other derivative datasets by applying a sequence of transformations",
      Seq("root", "derivative"),
      Some("root")
    ) match {
      case "root" =>
        val url = input("Source URL", "Specify URL where data is located.")(
          java.net.URI.create
        )

        // TODO: Add heuristics
        var prepareSteps = Vector.empty[PrepStepKind]

        if (inputYesNo("Is the source file compressed", "", false)) {
          val compression = inputChoice(
            "Compression",
            "What's the compression format?",
            Seq("zip", "gzip")
          )

          val subPathRegex = if (Seq("zip").contains(compression)) {
            inputOptional(
              "Sub-path",
              "If this archive can contain multiple files - specify the path regex to " +
                "help us find the right one."
            )(s => s)
          } else {
            None
          }

          prepareSteps = prepareSteps :+ PrepStepKind.Decompress(
            format = compression,
            subPathRegex = subPathRegex
          )
        }

        var format = inputChoice(
          "Format",
          "Specify which format is the source data in.",
          Seq("csv", "tsv", "json", "geojson", "shapefile")
        )

        val readerOptions = scala.collection.mutable.Map.empty[String, String]

        // TODO: Add heuristics
        if (format == "tsv") {
          format = "csv"
          readerOptions.put("delimiter", "\\t")
        }

        if (format == "csv") {
          if (inputYesNo("Is the first line a header", "", true))
            readerOptions.put("header", "true")
        }

        val mergeStrategy = inputChoice(
          "Merge strategy",
          "Merge strategy depends on the nature of the data you are using. If data contains " +
            "historical records which never change after being added and never deleted (data only grows " +
            "over time) - choose \"ledged\". If you are using data that changes over time " +
            "(e.g. daily database dumps) - choose \"shapshot\" and kamu will perform change data capture " +
            "on it to transform it into events. Or simply choose \"append\" and all data will be added " +
            "unchanged each time the source is updated.",
          Seq("snapshot", "ledger", "append")
        ) match {
          case "snapshot" =>
            // TODO: Column names
            val primaryKey = input(
              "PK column(s)",
              "Which columns uniquely identify the record throughout its lifetime (comma-separated)."
            )(s => s.split(',').map(_.trim).toVector)

            val compareColumns = inputOptional(
              "Columns to compare",
              "Names of the columns should be compared to determine if a row has changed. " +
                "For example this can be a modification timestamp, an incremental version, " +
                "or a data hash. If not specified all data columns will be compared one by one."
            )(s => s.split(',').map(_.trim).toVector).getOrElse(Vector.empty)

            MergeStrategyKind.Snapshot(
              primaryKey = primaryKey,
              compareColumns = compareColumns
            )

          case "ledger" =>
            // TODO: Column names
            val primaryKey = input(
              "PK Column",
              "Which columns uniquely identify the record throughout its lifetime (comma-separated)."
            )(s => s.split(',').map(_.trim).toVector)

            MergeStrategyKind.Ledger(primaryKey = primaryKey)
          case "append" =>
            MergeStrategyKind.Append()
        }

        DatasetSnapshot(
          id = id,
          source = SourceKind.Root(
            fetch = FetchSourceKind.Url(url = url),
            prepare = prepareSteps,
            read = ReaderKind
              .Generic(name = format, options = readerOptions.toMap),
            merge = mergeStrategy
          )
        )
      case "derivative" =>
        DatasetSnapshot(
          id = id,
          source = SourceKind.Derivative(
            inputs = Vector.empty,
            transform = yaml.saveObj(
              TransformKind.SparkSQL(
                engine = "sparkSQL",
                query = Some("SELECT * FROM input")
              )
            )
          )
        )
    }
  }

  ///////////////////////////////////////////////////////////////////////////////////////

  private def getPrompt(name: String, default: Option[String]) = {
    default.map(d => s"$name [$d]: ").getOrElse(s"$name: ")
  }

  private def input[T](
    name: String,
    help: String,
    default: Option[String] = None
  )(
    ctor: String => T
  ): T = {
    println()

    if (help.nonEmpty) {
      println(help)
      println()
    }

    retry {
      val s = StdIn.readLine(getPrompt(name, default))

      if (s.isEmpty) {
        if (default.isDefined)
          ctor(default.get)
        else
          throw new InvalidInputException("Requires a value")
      } else {
        try {
          ctor(s)
        } catch {
          case e: Exception =>
            throw new InvalidInputException(e.getMessage)
        }
      }
    }
  }

  private def inputOptional[T](name: String, help: String)(
    ctor: String => T
  ): Option[T] = {
    println()

    if (help.nonEmpty) {
      println(help)
      println()
    }

    retry {
      val s = StdIn.readLine(getPrompt(name, None))

      if (s.isEmpty) {
        None
      } else {
        try {
          Some(ctor(s))
        } catch {
          case e: Exception =>
            throw new InvalidInputException(e.getMessage)
        }
      }
    }
  }

  private def inputChoice(
    name: String,
    help: String,
    choices: Seq[String],
    default: Option[String] = None
  ): String = {
    println()

    if (help.nonEmpty) {
      println(help)
      println()
    }

    println(
      "Options:\n" + choices.zipWithIndex
        .map { case (c, i) => s" ${i + 1}) $c" }
        .mkString("\n")
    )
    println()

    retry {
      val s = StdIn.readLine(getPrompt(name, default))

      if (s.isEmpty) {
        if (default.isDefined)
          default.get
        else
          throw new InvalidInputException("Requires a value")
      } else if (choices.contains(s)) {
        s
      } else {
        try {
          val i = Integer.parseUnsignedInt(s) - 1
          if (i >= 0 && i < choices.size)
            return choices(i)
        } catch {
          case _: NumberFormatException =>
        }
        throw new InvalidInputException("Not one of the supported values")
      }
    }
  }

  private def inputYesNo(
    name: String,
    help: String,
    default: Boolean
  ): Boolean = {
    println()

    if (help.nonEmpty) {
      println(help)
      println()
    }

    val prompt = name + "[" + (if (default) "Y/n" else "y/N") + "]: "

    retry {
      val s = StdIn.readLine(prompt)

      if (s.isEmpty)
        default
      else if (s == "Y" || s == "y")
        true
      else if (s == "N" || s == "n")
        false
      else
        throw new InvalidInputException("Y or N please.")
    }
  }

  private def retry[T](fun: => T): T = {
    try {
      fun
    } catch {
      case e: InvalidInputException =>
        println()
        println(e.getMessage)
        println()
        retry(fun)
    }
  }

}
