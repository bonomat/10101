import 'package:flutter/material.dart';
import 'package:get_10101/common/color.dart';
import 'package:get_10101/logger/logger.dart';
import 'package:get_10101/trade/open_position_service.dart';
import 'package:get_10101/trade/quote_service.dart';
import 'package:intl/intl.dart';
import 'package:provider/provider.dart';

class OrderAndPositionTable extends StatefulWidget {
  const OrderAndPositionTable({super.key});

  @override
  OrderAndPositionTableState createState() => OrderAndPositionTableState();
}

class OrderAndPositionTableState extends State<OrderAndPositionTable>
    with SingleTickerProviderStateMixin {
  late final _tabController = TabController(length: 2, vsync: this);
  BestQuote? bestQuote;

  @override
  void initState() {
    super.initState();
    context.read<QuoteService>().fetchQuote().then((q) => setState(() {
          bestQuote = q;
        }));
  }

  @override
  Widget build(BuildContext context) {
    return Column(
      mainAxisAlignment: MainAxisAlignment.start,
      crossAxisAlignment: CrossAxisAlignment.center,
      children: <Widget>[
        TabBar(
          unselectedLabelColor: Colors.black,
          labelColor: tenTenOnePurple,
          controller: _tabController,
          isScrollable: false,
          tabs: const [
            Tab(
              text: 'Open',
            ),
            Tab(
              text: 'Pending',
            ),
          ],
        ),
        Expanded(
            child: TabBarView(
          controller: _tabController,
          children: const <Widget>[
            OpenPositionTable(),
            Text("Pending"),
          ],
        ))
      ],
    );
  }
}

class OpenPositionTable extends StatelessWidget {
  const OpenPositionTable({super.key});

  @override
  Widget build(BuildContext context) {
    return FutureBuilder<List<Position>>(
      future: OpenPositionsService.fetchOpenPositions(),
      builder: (context, snapshot) {
        if (snapshot.connectionState == ConnectionState.waiting) {
          return const Center(child: CircularProgressIndicator());
        } else if (snapshot.hasError) {
          logger.i("received ${snapshot.error}");
          return const Center(child: Text('Error loading data'));
        } else if (!snapshot.hasData || snapshot.data!.isEmpty) {
          return const Center(child: Text('No data available'));
        } else {
          return buildTable(snapshot.data!);
        }
      },
    );
  }

  Widget buildTable(List<Position> positions) {
    return Table(
      border: TableBorder.symmetric(inside: const BorderSide(width: 2, color: Colors.black)),
      defaultVerticalAlignment: TableCellVerticalAlignment.middle,
      columnWidths: const {
        0: MinColumnWidth(FixedColumnWidth(100.0), FractionColumnWidth(0.1)),
        1: MinColumnWidth(FixedColumnWidth(100.0), FractionColumnWidth(0.1)),
        2: MinColumnWidth(FixedColumnWidth(100.0), FractionColumnWidth(0.1)),
        3: MinColumnWidth(FixedColumnWidth(150.0), FractionColumnWidth(0.1)),
        4: MinColumnWidth(FixedColumnWidth(100.0), FractionColumnWidth(0.1)),
        5: MinColumnWidth(FixedColumnWidth(100.0), FractionColumnWidth(0.1)),
        6: MinColumnWidth(FixedColumnWidth(200.0), FractionColumnWidth(0.2)),
      },
      children: [
        TableRow(
          decoration: BoxDecoration(
            color: tenTenOnePurple.shade300,
            border: Border.all(
              width: 1,
            ),
            borderRadius: const BorderRadius.only(
                topLeft: Radius.circular(10), topRight: Radius.circular(10)),
          ),
          children: [
            buildHeaderCell('Quantity'),
            buildHeaderCell('Entry Price'),
            buildHeaderCell('Liquidation Price'),
            buildHeaderCell('Margin'),
            buildHeaderCell('Leverage'),
            buildHeaderCell('Unrealized PnL'),
            buildHeaderCell('Expiry'),
          ],
        ),
        for (var position in positions)
          TableRow(
            children: [
              buildTableCell(position.quantity.toString()),
              buildTableCell(position.averageEntryPrice.toString()),
              buildTableCell(position.liquidationPrice.toString()),
              buildTableCell(position.collateral.toString()),
              buildTableCell(position.leverage.formatted()),
              buildTableCell(position.pnlSats.toString()),
              buildTableCell("${DateFormat('dd-MM-yyyy – HH:mm').format(position.expiry)} CET"),
            ],
          ),
      ],
    );
  }

  TableCell buildHeaderCell(String text) {
    return TableCell(
        child: Container(
            padding: const EdgeInsets.all(10),
            alignment: Alignment.center,
            child: Text(text,
                textAlign: TextAlign.center,
                style: const TextStyle(fontWeight: FontWeight.bold, color: Colors.white))));
  }

  TableCell buildTableCell(String text) => TableCell(
      child: Center(
          child: Container(
              padding: const EdgeInsets.all(10), alignment: Alignment.center, child: Text(text))));
}
