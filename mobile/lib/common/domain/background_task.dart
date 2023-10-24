import 'package:get_10101/bridge_generated/bridge_definitions.dart' as bridge;
import 'package:get_10101/features/trade/domain/order.dart';

class AsyncTrade {
  final OrderReason orderReason;

  AsyncTrade({required this.orderReason});

  static AsyncTrade fromApi(bridge.BackgroundTask_AsyncTrade asyncTrade) {
    return AsyncTrade(orderReason: OrderReason.fromApi(asyncTrade.field0));
  }

  static bridge.BackgroundTask apiDummy() {
    return bridge.BackgroundTask_AsyncTrade(OrderReason.apiDummy());
  }
}

enum TaskStatus {
  pending,
  failed,
  success;

  static TaskStatus fromApi(bridge.TaskStatus taskStatus) {
    switch (taskStatus) {
      case bridge.TaskStatus.Pending:
        return TaskStatus.pending;
      case bridge.TaskStatus.Failed:
        return TaskStatus.failed;
      case bridge.TaskStatus.Success:
        return TaskStatus.success;
    }
  }

  static bridge.TaskStatus apiDummy() {
    return bridge.TaskStatus.Pending;
  }
}

class Rollover {
  final TaskStatus taskStatus;

  Rollover({required this.taskStatus});

  static Rollover fromApi(bridge.BackgroundTask_Rollover rollover) {
    return Rollover(taskStatus: TaskStatus.fromApi(rollover.field0));
  }

  static bridge.BackgroundTask apiDummy() {
    return bridge.BackgroundTask_Rollover(TaskStatus.apiDummy());
  }
}

class RecoverDlc {
  final TaskStatus taskStatus;

  RecoverDlc({required this.taskStatus});

  static RecoverDlc fromApi(bridge.BackgroundTask_RecoverDlc recoverDlc) {
    return RecoverDlc(taskStatus: TaskStatus.fromApi(recoverDlc.field0));
  }

  static bridge.BackgroundTask apiDummy() {
    return bridge.BackgroundTask_RecoverDlc(TaskStatus.apiDummy());
  }
}

class CollabRevert {
  final TaskStatus taskStatus;

  CollabRevert({required this.taskStatus});

  static CollabRevert fromApi(bridge.BackgroundTask_CollabRevert collabRevert) {
    return CollabRevert(taskStatus: TaskStatus.fromApi(collabRevert.field0));
  }

  static bridge.BackgroundTask apiDummy() {
    return bridge.BackgroundTask_CollabRevert(TaskStatus.apiDummy());
  }
}
