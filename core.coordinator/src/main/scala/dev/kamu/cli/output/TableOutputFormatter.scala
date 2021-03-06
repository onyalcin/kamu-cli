/*
 * Copyright (c) 2018 kamu.dev
 *
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/.
 */

package dev.kamu.cli.output

import java.io.PrintStream
import scala.math.max

class TableOutputFormatter(
  stream: PrintStream,
  outputFormat: OutputFormat,
  valueFormatter: ValueFormatter
) extends OutputFormatter {

  def format(rs: SimpleResultSet): Unit = {
    val maxColDataWidths = rs.columns.indices
      .map(
        i =>
          if (rs.rows.isEmpty) 0
          else rs.rows.map(row => valueFormatter.format(row(i)).length).max
      )
      .toArray

    val maxColWidths = rs.columns.indices
      .map(c => max(maxColDataWidths(c), rs.columns(c).length))

    if (outputFormat.withHeader)
      writeHeader(rs.columns, maxColWidths)

    rs.rows.foreach(row => {
      writeRow(row.map(valueFormatter.format), maxColWidths)
    })

    if (outputFormat.withHeader)
      writeSpacer(maxColWidths)
  }

  def writeHeader(
    values: Seq[String],
    widths: Seq[Int]
  ): Unit = {
    writeSpacer(widths)
    writeRow(values, widths)
    writeSpacer(widths)
  }

  def writeRow(
    values: Seq[String],
    widths: Seq[Int]
  ): Unit = {
    stream.print("| ")
    stream.print(
      values
        .zip(widths)
        .map {
          case (value, width) =>
            value + (" " * (width - value.length))
        }
        .mkString(" | ")
    )
    stream.println(" |")
  }

  def writeSpacer(widths: Seq[Int]): Unit = {
    stream.print("+-")
    stream.print(widths.map("-" * _).mkString("-+-"))
    stream.println("-+")
  }
}
